//! Rust values LLDB shows poorly, made readable like RustRover's: enums (`Option`, `Result`,
//! your own) read `Some(5)`, `Err("bad")`, `Rect { w: 3, h: 4 }`; arrays and structs without a
//! formatter read `[1, 2, 3]` / `Point { x: 1, y: 2 }` instead of `T @ 0x…`; a `Box` of a
//! number reads its value.
//!
//! LLDB without Rust language support (Xcode's) shows an enum's DWARF variant part as a
//! `$variants$` child holding one `$variant$N` per variant (`N` its discriminant value, none for
//! the niche/default variant), each `{$discr$:D, value:{…}}` with the same discriminant `D`
//! read from memory and the variant's name in its type (`…::Some$Variant`). The active variant
//! is the one whose `N` is `D` (LLDB truncates `N` to 32 bits), else the default one.
//!
//! Children are fetched after the scope's variables arrive (two to four small requests per
//! value); expanding an enum then shows the active variant's fields.
//!
//! M15 (track DM): a value nested in a variant (`Some(Some(5))`, `Wrap(B { x: 9 })`) is decoded
//! too and the outer summary rebuilt from it, up to [`MAX_DEPTH`] levels; LLDB shows `u8`/`i8`
//! as C characters (`'A'`, `'\x05'`) and a `Box<u8>` or `[u8; N]` as a C string: they read as
//! numbers (`65`, `5`, `[1, 2, 255]`), while a Rust `char` (`U+0x7a U'z'`) reads `'z'`.

use serde_json::Value;

use codecraft_dap as dap;

use super::{Pending, VariableInfo};
use crate::Session;

/// Values summarised per scope (the rest keep LLDB's text).
const MAX_PER_SCOPE: usize = 64;
const MAX_SUMMARY_CHARS: usize = 120;
const MAX_ITEMS: usize = 10;
/// How deep values nested in enum variants (and in what those hold) are decoded.
const MAX_DEPTH: u8 = 3;

/// Where a summary goes: the variable `name` (with `reference`) among the children of `parent`.
#[derive(Clone, Debug)]
pub(crate) struct Slot {
    pub parent: i64,
    pub name: String,
    pub reference: i64,
    pub type_name: Option<String>,
    /// The enum variant this value is a field of (its summary is rebuilt from the fields).
    pub outer: Option<Box<Outer>>,
    pub depth: u8,
}

/// An enum value whose active variant holds the slot's value.
#[derive(Clone, Debug)]
pub(crate) struct Outer {
    pub slot: Slot,
    pub variant: String,
}

#[derive(Clone, Debug)]
pub(crate) enum Stage {
    /// The value's own children (`$variants$`, array items, struct fields, a pointee).
    Outer,
    /// The `$variant$N` children of `$variants$`.
    Variants,
    /// The active variant's children (`$discr$`, `value`).
    Variant { variant: String },
    /// The active variant's fields (what expanding the enum shows).
    Fields { variant: String },
}

/// A Rust scalar as Rust prints it: `u8`/`i8` (which LLDB shows as C `unsigned char`/`char`
/// literals) as numbers, a `char` (`U+0x0000007a U'z'`) as `'z'`. Other values are unchanged.
pub fn scalar(type_name: Option<&str>, value: String) -> String {
    let ty = type_name.map(str::trim).unwrap_or_default();
    if value.starts_with("U+") && value.contains("U'") {
        return rust_char(&value).unwrap_or(value);
    }
    let literal = value.trim_start().starts_with('\'');
    let byte = if literal { parse_discr(&value).map(|n| (n & 0xFF) as u8) } else { None };
    match (ty, byte) {
        ("unsigned char" | "u8" | "uint8_t", Some(b)) => b.to_string(),
        ("char" | "signed char" | "i8" | "int8_t", Some(b)) => i8::from_ne_bytes([b]).to_string(),
        _ => value,
    }
}

/// `U+0x0000007a U+0x0000007a U'z'` → `'z'`.
fn rust_char(value: &str) -> Option<String> {
    let at = value.rfind("U'")?;
    let lit = value.get(at + 1..)?.trim();
    (lit.len() >= 3 && lit.ends_with('\'')).then(|| lit.to_owned())
}

/// A Rust byte type as LLDB names it.
fn is_byte_type(ty: &str) -> bool {
    matches!(ty.trim(), "unsigned char" | "char" | "signed char" | "u8" | "i8")
}

/// `unsigned char *` (a `Box<u8>`, `*const u8`) or `unsigned char[3]` (`[u8; 3]`), which LLDB
/// shows as a C string.
fn is_byte_pointer_or_array(ty: &str) -> bool {
    let t = ty.trim();
    if let Some(base) = t.strip_suffix('*') {
        let base = base.trim();
        let base = base.strip_prefix("const ").unwrap_or(base).trim_end_matches("const").trim();
        return is_byte_type(base);
    }
    t.rsplit_once('[').is_some_and(|(base, n)| is_byte_type(base) && n.trim_end_matches(']').chars().all(|c| c.is_ascii_digit()))
}

/// `T @ 0x…`: LLDB's text for an aggregate without a summary.
fn is_bare_aggregate(value: &str) -> bool {
    value.rsplit_once(" @ 0x").is_some_and(|(t, hex)| !t.is_empty() && !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

/// A bare pointer (`0x0000600000c04000`), as a `Box<i32>` reads.
fn is_bare_pointer(value: &str) -> bool {
    value.strip_prefix("0x").is_some_and(|hex| !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

/// A value LLDB shows as just its type's name (an enum or struct nested in an enum's variant).
fn is_type_name_only(v: &VariableInfo) -> bool {
    v.type_name.as_deref().is_some_and(|t| !t.is_empty() && v.value.trim() == t.trim())
}

/// Whether `v`'s children make a better summary than LLDB's text.
fn wants_summary(v: &VariableInfo) -> bool {
    let ty = v.type_name.as_deref().unwrap_or_default();
    v.reference > 0
        && (is_bare_aggregate(&v.value)
            || (is_bare_pointer(&v.value) && ty.ends_with('*'))
            || is_type_name_only(v)
            || (is_byte_pointer_or_array(ty) && (v.value.starts_with('"') || is_bare_pointer(&v.value))))
}

/// Ask for the children of the variables of `parent` that need a summary.
pub(crate) fn summarize(s: &mut Session, parent: i64, vars: &[VariableInfo]) {
    summarize_in(s, parent, vars, None, 0);
}

/// [`summarize`] for the fields of an enum's variant (`outer`), `depth` levels down.
fn summarize_in(s: &mut Session, parent: i64, vars: &[VariableInfo], outer: Option<Outer>, depth: u8) {
    if depth > MAX_DEPTH {
        return;
    }
    let wanted: Vec<Slot> = vars
        .iter()
        .filter(|v| wants_summary(v))
        .take(MAX_PER_SCOPE)
        .map(|v| Slot {
            parent,
            name: v.name.clone(),
            reference: v.reference,
            type_name: v.type_name.clone(),
            outer: outer.clone().map(Box::new),
            depth,
        })
        .collect();
    for slot in wanted {
        let reference = slot.reference;
        request(s, slot, Stage::Outer, reference);
    }
}

fn request(s: &mut Session, slot: Slot, stage: Stage, reference: i64) {
    let Some(rt) = s.debug.rt.as_mut() else { return };
    let epoch = rt.epoch;
    let request = super::actions::ActionRequest::RustValue { slot, stage };
    rt.request(Pending::Action { epoch, request }, |c| c.variables(reference));
}

fn children(result: Result<Value, String>) -> Vec<dap::Variable> {
    result.ok().map(|body| dap::types::parse::<dap::VariablesBody>(&body).unwrap_or_default().variables).unwrap_or_default()
}

/// The answer to one of the requests above.
pub(crate) fn on_response(s: &mut Session, current: bool, slot: Slot, stage: Stage, result: Result<Value, String>) {
    if !current {
        return;
    }
    let kids = children(result);
    match stage {
        Stage::Outer => {
            if let [only] = kids.as_slice()
                && only.name == "$variants$"
                && only.variables_reference > 0
            {
                return request(s, slot, Stage::Variants, only.variables_reference);
            }
            let summary = match kids.as_slice() {
                [pointee] if pointee.name.starts_with('*') => {
                    Some(scalar(pointee.type_.as_deref(), super::rust_value(pointee.value.clone())))
                }
                _ if kids.is_empty() => None,
                _ => Some(aggregate_summary(slot.type_name.as_deref(), &kids)),
            };
            if let Some(summary) = summary {
                set_value(s, &slot, summary);
            }
        }
        Stage::Variants => {
            let Some(active) = active_variant(&kids) else { return };
            let name = variant_name(active.type_.as_deref().unwrap_or_default());
            let fields = active_fields(&active.value);
            set_value(s, &slot, shorten(&enum_summary(&name, &fields)));
            if active.variables_reference > 0 {
                let reference = active.variables_reference;
                request(s, slot, Stage::Variant { variant: name }, reference);
            }
        }
        Stage::Variant { variant } => match kids.iter().find(|k| k.name == "value") {
            Some(value) if value.variables_reference > 0 => {
                request(s, slot, Stage::Fields { variant }, value.variables_reference)
            }
            _ => set_children(s, &slot, Vec::new()),
        },
        Stage::Fields { variant } => {
            let fields: Vec<VariableInfo> = kids
                .into_iter()
                .map(|v| {
                    let type_name = v.type_.filter(|t| !t.is_empty());
                    VariableInfo {
                        name: v.name,
                        value: scalar(type_name.as_deref(), super::rust_value(v.value)),
                        type_name,
                        reference: v.variables_reference,
                        evaluate_name: v.evaluate_name,
                        memory_reference: v.memory_reference,
                    }
                })
                .collect();
            let reference = slot.reference;
            set_children(s, &slot, fields.clone());
            // The typed fields read better than LLDB's text (bytes as numbers).
            set_value(s, &slot, shorten(&enum_summary(&variant, &field_pairs(&fields))));
            // Fields can be enums, structs or arrays too: decoded one level deeper, and this
            // summary rebuilt from them.
            let depth = slot.depth.saturating_add(1);
            summarize_in(s, reference, &fields, Some(Outer { slot, variant }), depth);
        }
    }
}

/// A variant's fields for its summary (values LLDB names only by their type read `{…}` until
/// they are decoded).
fn field_pairs(fields: &[VariableInfo]) -> Vec<(String, String)> {
    fields
        .iter()
        .map(|f| {
            let value = if is_type_name_only(f) || is_bare_aggregate(&f.value) { "{…}".to_owned() } else { f.value.clone() };
            (f.name.clone(), value)
        })
        .collect()
}

/// Replace the value of the slot's variable (and the inline values when it is a local); a
/// field of an enum's variant rebuilds that enum's summary.
fn set_value(s: &mut Session, slot: &Slot, value: String) {
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
    if let Some(outer) = &slot.outer {
        let fields = s.debug.session.as_ref().and_then(|x| x.variables.get(&slot.parent)).cloned().unwrap_or_default();
        let summary = shorten(&enum_summary(&outer.variant, &field_pairs(&fields)));
        set_value(s, &outer.slot, summary);
    }
}

/// What expanding the slot's variable shows.
fn set_children(s: &mut Session, slot: &Slot, fields: Vec<VariableInfo>) {
    if let Some(session) = s.debug.session.as_mut() {
        session.loading.remove(&slot.reference);
        session.variables.insert(slot.reference, fields);
    }
}

/// The variant the discriminant selects.
fn active_variant(variants: &[dap::Variable]) -> Option<&dap::Variable> {
    let discr = variants.iter().find_map(|v| field(&v.value, "$discr$").and_then(|d| parse_discr(&d)));
    let numbered = |v: &dap::Variable| v.name.strip_prefix("$variant$").and_then(|n| n.parse::<u64>().ok());
    if let Some(d) = discr
        && let Some(v) = variants.iter().find(|v| numbered(v).is_some_and(|n| n == d || n == d & 0xFFFF_FFFF))
    {
        return Some(v);
    }
    variants.iter().find(|v| v.name == "$variant$").or_else(|| if variants.len() == 1 { variants.first() } else { None })
}

/// `…::Some$Variant` → `Some`.
fn variant_name(type_name: &str) -> String {
    let last = type_name.rsplit("::").next().unwrap_or(type_name);
    last.strip_suffix("$Variant").unwrap_or(last).to_owned()
}

/// A discriminant as LLDB prints it: a number or a character (`'\x01'`, `'a'`).
fn parse_discr(text: &str) -> Option<u64> {
    let t = text.trim();
    if let Ok(n) = t.parse::<u64>() {
        return Some(n);
    }
    if let Ok(n) = t.parse::<i64>() {
        return Some(n as u64);
    }
    let inner = t.strip_prefix('\'')?.strip_suffix('\'')?;
    match inner {
        "\\0" => Some(0),
        "\\a" => Some(7),
        "\\b" => Some(8),
        "\\t" => Some(9),
        "\\n" => Some(10),
        "\\v" => Some(11),
        "\\f" => Some(12),
        "\\r" => Some(13),
        "\\e" => Some(27),
        "\\\\" => Some(92),
        "\\'" => Some(39),
        "\\\"" => Some(34),
        _ => match inner.strip_prefix("\\x").or_else(|| inner.strip_prefix("\\u")).or_else(|| inner.strip_prefix("\\U")) {
            Some(hex) => u64::from_str_radix(hex, 16).ok(),
            None => {
                let mut chars = inner.chars();
                let c = chars.next()?;
                chars.next().is_none().then_some(u64::from(u32::from(c)))
            }
        },
    }
}

/// The fields of the variant's `value` (`{$discr$:1, value:{0:5}}` → `[(0, 5)]`).
fn active_fields(variant_value: &str) -> Vec<(String, String)> {
    field(variant_value, "value").map(|v| fields(&v)).unwrap_or_default()
}

/// The text of field `name` in `{a:1, b:{…}}`.
fn field(text: &str, name: &str) -> Option<String> {
    fields(text).into_iter().find(|(n, _)| n == name).map(|(_, v)| v)
}

/// Split LLDB's `{name:value, name:value}` at its top-level commas (quotes and nesting kept).
pub fn fields(text: &str) -> Vec<(String, String)> {
    let t = text.trim();
    let Some(inner) = t.strip_prefix('{').and_then(|r| r.strip_suffix('}')) else { return Vec::new() };
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut current = String::new();
    let push = |current: &mut String, out: &mut Vec<(String, String)>| {
        let part = std::mem::take(current);
        let part = part.trim();
        if part.is_empty() {
            return;
        }
        match part.split_once(':') {
            Some((name, value)) if !name.contains(['{', '"', '\'']) => {
                out.push((name.trim().to_owned(), value.trim().to_owned()));
            }
            _ => out.push((String::new(), part.to_owned())),
        }
    };
    for c in inner.chars() {
        if let Some(q) = quote {
            current.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                current.push(c);
            }
            '{' | '[' | '(' => {
                depth += 1;
                current.push(c);
            }
            '}' | ']' | ')' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ',' if depth == 0 => push(&mut current, &mut out),
            _ => current.push(c),
        }
    }
    push(&mut current, &mut out);
    out
}

/// `Some(5)`, `Rect { w: 3, h: 4 }`, `Empty`.
pub fn enum_summary(variant: &str, fields: &[(String, String)]) -> String {
    let tuple = fields.iter().all(|(n, _)| n.parse::<usize>().is_ok() || n.is_empty());
    if fields.is_empty() {
        variant.to_owned()
    } else if tuple {
        let items: Vec<String> = fields.iter().map(|(_, v)| tidy(v)).collect();
        format!("{variant}({})", items.join(", "))
    } else {
        let items: Vec<String> = fields.iter().map(|(n, v)| format!("{n}: {}", tidy(v))).collect();
        format!("{variant} {{ {} }}", items.join(", "))
    }
}

/// A nested enum's raw text means nothing to a reader: say it's a value.
fn tidy(value: &str) -> String {
    if value.contains("$variant") || value.contains("$discr$") { "{…}".to_owned() } else { value.to_owned() }
}

/// `[1, 2, 3]` for array items, `Point { x: 1, y: 2 }` for fields.
fn aggregate_summary(type_name: Option<&str>, kids: &[dap::Variable]) -> String {
    let indexed = kids.iter().all(|k| k.name.starts_with('[') && k.name.ends_with(']'));
    let mut items: Vec<String> = kids
        .iter()
        .take(MAX_ITEMS)
        .map(|k| {
            let value = tidy(&scalar(k.type_.as_deref(), super::rust_value(k.value.clone())));
            let named_only = k.type_.as_deref().is_some_and(|t| !t.is_empty() && t.trim() == value.trim());
            let value = if is_bare_aggregate(&value) || named_only { "{…}".to_owned() } else { value };
            if indexed { value } else { format!("{}: {value}", k.name) }
        })
        .collect();
    if kids.len() > MAX_ITEMS {
        items.push("…".into());
    }
    let summary = if indexed {
        format!("[{}]", items.join(", "))
    } else {
        let short = type_name.map(short_type).unwrap_or_default();
        if short.is_empty() { format!("{{ {} }}", items.join(", ")) } else { format!("{short} {{ {} }}", items.join(", ")) }
    };
    shorten(&summary)
}

/// `my_crate::geo::Point<i32>` → `Point`.
fn short_type(full: &str) -> String {
    let base = full.split('<').next().unwrap_or(full);
    base.rsplit("::").next().unwrap_or(base).trim().to_owned()
}

fn shorten(text: &str) -> String {
    let mut out: String = text.chars().take(MAX_SUMMARY_CHARS).collect();
    if text.chars().count() > MAX_SUMMARY_CHARS {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(name: &str, ty: &str, value: &str) -> dap::Variable {
        dap::Variable { name: name.into(), value: value.into(), type_: Some(ty.into()), ..Default::default() }
    }

    #[test]
    fn enum_variants_decode_like_rust_debug() {
        let some = [
            var("$variant$0", "Option<i32>$Inner::None$Variant", "{$discr$:1, value:{}}"),
            var("$variant$1", "Option<i32>$Inner::Some$Variant", "{$discr$:1, value:{0:5}}"),
        ];
        let v = active_variant(&some).unwrap();
        assert_eq!(enum_summary(&variant_name(v.type_.as_deref().unwrap()), &active_fields(&v.value)), "Some(5)");
        let none = [
            var("$variant$0", "Option<i32>$Inner::None$Variant", "{$discr$:0, value:{}}"),
            var("$variant$1", "Option<i32>$Inner::Some$Variant", "{$discr$:0, value:{0:1}}"),
        ];
        assert_eq!(variant_name(active_variant(&none).unwrap().type_.as_deref().unwrap()), "None");
        // Niche layouts: the 32-bit truncated name, else the default variant.
        let ok = [
            var("$variant$4294967295", "Result$Inner::Ok$Variant", "{$discr$:18446744073709551615, value:{0:7}}"),
            var("$variant$", "Result$Inner::Err$Variant", "{value:{0:<error>}}"),
        ];
        let v = active_variant(&ok).unwrap();
        assert_eq!(enum_summary(&variant_name(v.type_.as_deref().unwrap()), &active_fields(&v.value)), "Ok(7)");
        let err = [
            var("$variant$4294967295", "Result$Inner::Ok$Variant", "{$discr$:3, value:{0:54149136}}"),
            var("$variant$", "Result$Inner::Err$Variant", "{value:{0:\"bad, {x}\"}}"),
        ];
        let v = active_variant(&err).unwrap();
        assert_eq!(enum_summary(&variant_name(v.type_.as_deref().unwrap()), &active_fields(&v.value)), "Err(\"bad, {x}\")");
        let rect = [
            var("$variant$0", "Shape$Inner::Circle$Variant", "{$discr$:1, value:{0:2.1E-314}}"),
            var("$variant$1", "Shape$Inner::Rect$Variant", "{$discr$:1, value:{w:3, h:4}}"),
        ];
        let v = active_variant(&rect).unwrap();
        assert_eq!(enum_summary(&variant_name(v.type_.as_deref().unwrap()), &active_fields(&v.value)), "Rect { w: 3, h: 4 }");
        let byte = [
            var("$variant$0", "E$Inner::A$Variant", "{$discr$:'\\0', value:{}}"),
            var("$variant$1", "E$Inner::B$Variant", "{$discr$:'\\x01', value:{}}"),
        ];
        assert_eq!(variant_name(active_variant(&byte[..]).unwrap().type_.as_deref().unwrap()), "A");
        assert!(active_variant(&[]).is_none());
    }

    #[test]
    fn bytes_read_as_numbers_and_chars_as_chars() {
        let s = |ty: &str, v: &str| scalar(Some(ty), v.to_owned());
        assert_eq!(s("unsigned char", "'A'"), "65");
        assert_eq!(s("unsigned char", "'\\a'"), "7");
        assert_eq!(s("unsigned char", "'\\xff'"), "255");
        assert_eq!(s("unsigned char", "'\\0'"), "0");
        assert_eq!(s("char", "'\\xfd'"), "-3");
        assert_eq!(s("char", "'a'"), "97");
        assert_eq!(s("u8", "'\\x05'"), "5");
        // A Rust char: the character only.
        assert_eq!(s("char32_t", "U+0x0000007a U+0x0000007a U'z'"), "'z'");
        assert_eq!(s("char32_t", "U+0x000000e9 U'é'"), "'é'");
        // Other values and hostile text stay as they are.
        assert_eq!(s("unsigned char", "65"), "65");
        assert_eq!(s("unsigned char", "'ab'"), "'ab'");
        assert_eq!(s("i32", "'A'"), "'A'");
        assert_eq!(s("char32_t", "U+"), "U+");
        assert_eq!(scalar(None, "'x'".into()), "'x'");
        assert!(is_byte_pointer_or_array("unsigned char *"));
        assert!(is_byte_pointer_or_array("const unsigned char *"));
        assert!(is_byte_pointer_or_array("unsigned char[3]"));
        assert!(!is_byte_pointer_or_array("int[3]"));
        assert!(!is_byte_pointer_or_array("unsigned char[x]"));
        assert!(!is_byte_pointer_or_array("int *"));
        // A `Box<u8>`'s C-string text and a nested enum's type-name text get a summary.
        let boxed = VariableInfo {
            name: "b".into(),
            value: "\"\\a\"".into(),
            type_name: Some("unsigned char *".into()),
            reference: 8,
            evaluate_name: None,
            memory_reference: None,
        };
        assert!(wants_summary(&boxed));
        let nested = VariableInfo { value: "core::option::Option<i32>".into(), type_name: Some("core::option::Option<i32>".into()), ..boxed.clone() };
        assert!(wants_summary(&nested));
        assert_eq!(field_pairs(std::slice::from_ref(&nested)), vec![("b".to_owned(), "{…}".to_owned())]);
        let decoded = VariableInfo { value: "Some(5)".into(), ..nested };
        assert_eq!(enum_summary("Some", &field_pairs(&[VariableInfo { name: "0".into(), ..decoded }])), "Some(Some(5))");
        let plain = VariableInfo { value: "5".into(), type_name: Some("i32".into()), reference: 0, ..boxed };
        assert!(!wants_summary(&plain));
    }

    #[test]
    fn the_rust_plugin_warning_is_dropped() {
        let out = super::super::without_rust_plugin_warning(
            "warning: This version of LLDB has no plugin for the language \"rust\". Inspection of frame variables will be limited.\nhello\n",
        );
        assert_eq!(out, "hello\n");
        assert_eq!(super::super::without_rust_plugin_warning("plain output"), "plain output");
        assert_eq!(super::super::without_rust_plugin_warning("no plugin for the language \"swift\"\n"), "no plugin for the language \"swift\"\n");
    }

    #[test]
    fn fields_split_at_top_level_only() {
        assert_eq!(fields("{a:1, b:{c:2, d:3}, e:\"x,}\"}").len(), 3);
        assert_eq!(fields("{}"), Vec::<(String, String)>::new());
        assert_eq!(fields("not braces"), Vec::<(String, String)>::new());
        assert_eq!(fields("{0:'}'}"), vec![("0".to_owned(), "'}'".to_owned())]);
        assert_eq!(parse_discr("-1"), Some(u64::MAX));
        assert_eq!(parse_discr("'a'"), Some(97));
        assert_eq!(parse_discr("''"), None);
    }

    #[test]
    fn aggregates_and_pointers() {
        assert!(is_bare_aggregate("int[4] @ 0x16fdfc1b8"));
        assert!(!is_bare_aggregate("size=3"));
        assert!(!is_bare_aggregate(" @ 0x1"));
        assert!(is_bare_pointer("0x0000600000c04000"));
        assert!(!is_bare_pointer("0x"));
        let items: Vec<dap::Variable> = (0..12).map(|i| var(&format!("[{i}]"), "int", &i.to_string())).collect();
        assert_eq!(aggregate_summary(Some("int[12]"), &items), "[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, …]");
        let point = [var("x", "i32", "1"), var("y", "i32", "2")];
        assert_eq!(aggregate_summary(Some("geo::Point<i32>"), &point), "Point { x: 1, y: 2 }");
        assert!(shorten(&"x".repeat(500)).chars().count() <= MAX_SUMMARY_CHARS + 1);
    }
}
