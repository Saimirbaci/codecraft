//! Live templates: abbreviations that expand into code (Tab, Code › Insert Live Template…) and
//! selection templates (Code › Surround With…).
//!
//! Template text uses `$NAME$` variables (Tab and Shift+Tab move between them; repeated names
//! are edited together), `$END$` (the caret after the last variable), `$SELECTION$` (the selected
//! text, for Surround With) and `$$` for a literal `$`. A tab at a line start is one indent level
//! of the file; every line after the first gets the indentation of the line the template is
//! expanded on.
//!
//! The built-in set is CodeCraft's own (written for this project, not taken from RustRover).
//! User templates live in `templates.json` under `$CODECRAFT_HOME` (default `~/.codecraft`) and
//! override built-ins with the same abbreviation and language.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use codecraft_syntax::Language;

/// One live template.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    pub abbreviation: String,
    pub description: String,
    pub text: String,
    /// Language ids where the template applies (`rust`, `python`, `typescript`, `javascript`,
    /// `c`, `cpp`, `zig`, …, as [`Language::lsp_id`]); `any` matches every language.
    pub languages: Vec<String>,
    /// Initial values of variables by name (empty when missing).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub defaults: BTreeMap<String, String>,
    /// Expressions that compute a variable's initial value (`fileNameWithoutExtension()`,
    /// `snakeCase(NAME)`, `date()`, `"text"`…; see [`eval_expression`]); a default applies when
    /// the expression gives nothing.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub expressions: BTreeMap<String, String>,
    /// Variables Tab skips when they already have a value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip_if_defined: Vec<String>,
    /// Off: not offered (a disabled copy of a built-in hides the built-in).
    #[serde(default = "enabled_default", skip_serializing_if = "is_enabled")]
    pub enabled: bool,
}

fn enabled_default() -> bool {
    true
}

fn is_enabled(b: &bool) -> bool {
    *b
}

impl Template {
    pub fn new(abbreviation: &str, description: &str, text: &str, languages: &[&str]) -> Self {
        Self {
            abbreviation: abbreviation.to_string(),
            description: description.to_string(),
            text: text.to_string(),
            languages: languages.iter().map(|l| (*l).to_string()).collect(),
            defaults: BTreeMap::new(),
            expressions: BTreeMap::new(),
            skip_if_defined: Vec::new(),
            enabled: true,
        }
    }

    /// Whether `self` and `other` have the same abbreviation in at least one common language
    /// (a user template shadows a built-in this way).
    pub fn overlaps(&self, other: &Template) -> bool {
        self.abbreviation == other.abbreviation
            && (self.languages.iter().any(|l| l == "any" || other.languages.contains(l))
                || other.languages.iter().any(|l| l == "any"))
    }

    fn with_default(mut self, name: &str, value: &str) -> Self {
        self.defaults.insert(name.to_string(), value.to_string());
        self
    }

    /// Applies to files of `language`.
    pub fn applies_to(&self, language: Language) -> bool {
        let id = language.lsp_id();
        self.languages.iter().any(|l| l == "any" || l == id)
    }

    /// A selection template (offered by Surround With).
    pub fn is_surround(&self) -> bool {
        self.text.contains("$SELECTION$")
    }

    /// Variable names in order of first appearance (without `END` and `SELECTION`).
    pub fn variables(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for seg in segments(&self.text) {
            if let Segment::Var(name) = seg
                && !out.contains(&name)
            {
                out.push(name);
            }
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    Text(String),
    Var(String),
    End,
    Selection,
}

fn is_var_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Split template text into literal text and placeholders. A `$` that does not start a valid
/// `$NAME$` is literal.
fn segments(text: &str) -> Vec<Segment> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if c != '$' {
            lit.push(c);
            i += 1;
            continue;
        }
        if chars.get(i + 1) == Some(&'$') {
            lit.push('$');
            i += 2;
            continue;
        }
        let mut j = i + 1;
        while chars.get(j).is_some_and(|c| is_var_char(*c)) {
            j += 1;
        }
        if j > i + 1 && chars.get(j) == Some(&'$') {
            if !lit.is_empty() {
                out.push(Segment::Text(std::mem::take(&mut lit)));
            }
            let name: String = chars.get(i + 1..j).unwrap_or(&[]).iter().collect();
            out.push(match name.as_str() {
                "END" => Segment::End,
                "SELECTION" => Segment::Selection,
                _ => Segment::Var(name),
            });
            i = j + 1;
        } else {
            lit.push('$');
            i += 1;
        }
    }
    if !lit.is_empty() {
        out.push(Segment::Text(lit));
    }
    out
}

/// Where and how a template is expanded.
#[derive(Clone, Debug, Default)]
pub struct ExpandContext {
    /// Indentation of the line the template is inserted on.
    pub indent: String,
    /// One indent level of the file (`"    "` or `"\t"`).
    pub indent_unit: String,
    /// Selected text for `$SELECTION$` (already dedented).
    pub selection: Option<String>,
    /// The file's name, for `fileName()` / `fileNameWithoutExtension()`.
    pub file_name: Option<String>,
    /// Zero-based line the template is expanded on (`lineNumber()` gives it 1-based).
    pub line: usize,
    /// Seconds since the Unix epoch for `date()` / `time()`; `None` = now.
    pub now: Option<u64>,
}

/// An expanded template: text to insert and placeholder positions (char offsets in `text`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expansion {
    pub text: String,
    /// One entry per variable (in order), with every occurrence.
    pub vars: Vec<Vec<Range<usize>>>,
    pub end: Option<usize>,
    /// Per variable: Tab skips it (it is in `skip_if_defined` and has a value).
    pub skip: Vec<bool>,
}

// ------------------------------------------------------------------------------- expressions

/// The functions a variable expression may call.
pub const EXPRESSION_FUNCTIONS: &[&str] = &[
    "date",
    "time",
    "user",
    "fileName",
    "fileNameWithoutExtension",
    "lineNumber",
    "capitalize",
    "decapitalize",
    "lowercase",
    "uppercase",
    "camelCase",
    "snakeCase",
    "spaceSeparated",
];

#[derive(Clone, Debug, PartialEq)]
enum Expr {
    Text(String),
    Var(String),
    Call(String, Option<Box<Expr>>),
}

/// Most nesting an expression may have.
const MAX_EXPR_DEPTH: usize = 16;

fn parse_expr(src: &str) -> Result<Expr, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut pos = 0;
    let e = parse_at(&chars, &mut pos, 0)?;
    skip_ws(&chars, &mut pos);
    if pos != chars.len() {
        return Err(format!("unexpected `{}` in `{src}`", chars.get(pos..).unwrap_or(&[]).iter().collect::<String>()));
    }
    Ok(e)
}

fn skip_ws(chars: &[char], pos: &mut usize) {
    while chars.get(*pos).is_some_and(|c| c.is_whitespace()) {
        *pos += 1;
    }
}

fn parse_at(chars: &[char], pos: &mut usize, depth: usize) -> Result<Expr, String> {
    if depth > MAX_EXPR_DEPTH {
        return Err("the expression is nested too deeply".into());
    }
    skip_ws(chars, pos);
    match chars.get(*pos) {
        Some('"') => {
            *pos += 1;
            let mut s = String::new();
            loop {
                match chars.get(*pos) {
                    Some('"') => {
                        *pos += 1;
                        return Ok(Expr::Text(s));
                    }
                    Some('\\') => {
                        if let Some(c) = chars.get(*pos + 1) {
                            s.push(*c);
                        }
                        *pos += 2;
                    }
                    Some(c) => {
                        s.push(*c);
                        *pos += 1;
                    }
                    None => return Err("unterminated string in the expression".into()),
                }
            }
        }
        Some(c) if c.is_alphabetic() || *c == '_' => {
            let start = *pos;
            while chars.get(*pos).is_some_and(|c| is_var_char(*c)) {
                *pos += 1;
            }
            let name: String = chars.get(start..*pos).unwrap_or(&[]).iter().collect();
            skip_ws(chars, pos);
            if chars.get(*pos) != Some(&'(') {
                return Ok(Expr::Var(name));
            }
            *pos += 1;
            if !EXPRESSION_FUNCTIONS.contains(&name.as_str()) {
                return Err(format!("unknown function `{name}()`; use {}", EXPRESSION_FUNCTIONS.join(", ")));
            }
            skip_ws(chars, pos);
            let arg = if chars.get(*pos) == Some(&')') { None } else { Some(Box::new(parse_at(chars, pos, depth + 1)?)) };
            skip_ws(chars, pos);
            if chars.get(*pos) != Some(&')') {
                return Err(format!("`{name}(` is missing its `)`"));
            }
            *pos += 1;
            Ok(Expr::Call(name, arg))
        }
        Some(c) => Err(format!("unexpected `{c}` in the expression")),
        None => Err("empty expression".into()),
    }
}

/// Check an expression's syntax (empty text is allowed: no expression).
pub fn check_expression(src: &str) -> Result<(), String> {
    if src.trim().is_empty() {
        return Ok(());
    }
    if src.chars().count() > 500 {
        return Err("the expression is too long".into());
    }
    parse_expr(src).map(|_| ())
}

/// Words of an identifier-like text (`fooBar_baz qux` → foo, bar, baz, qux).
fn words(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// Civil date `(year, month, day)` and `(hour, minute)` in UTC from Unix seconds.
fn civil(secs: u64) -> ((i64, u32, u32), (u64, u64)) {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Howard Hinnant's days-from-civil inverse.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (((if m <= 2 { y + 1 } else { y }), m, d), (rem / 3600, (rem % 3600) / 60))
}

fn eval(e: &Expr, ctx: &ExpandContext, vars: &BTreeMap<String, String>) -> String {
    let arg = |a: &Option<Box<Expr>>| a.as_deref().map(|a| eval(a, ctx, vars)).unwrap_or_default();
    match e {
        Expr::Text(s) => s.clone(),
        Expr::Var(name) => vars.get(name).cloned().unwrap_or_default(),
        Expr::Call(f, a) => match f.as_str() {
            "date" | "time" => {
                let now = ctx.now.unwrap_or_else(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                });
                let ((y, m, d), (h, min)) = civil(now);
                if f == "date" { format!("{y:04}-{m:02}-{d:02}") } else { format!("{h:02}:{min:02}") }
            }
            "user" => std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default(),
            "fileName" => ctx.file_name.clone().unwrap_or_default(),
            "fileNameWithoutExtension" => {
                let n = ctx.file_name.clone().unwrap_or_default();
                match n.rsplit_once('.') {
                    Some((stem, _)) if !stem.is_empty() => stem.to_string(),
                    _ => n,
                }
            }
            "lineNumber" => (ctx.line + 1).to_string(),
            "capitalize" => capitalize(&arg(a)),
            "decapitalize" => {
                let s = arg(a);
                let mut c = s.chars();
                c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or_default()
            }
            "lowercase" => arg(a).to_lowercase(),
            "uppercase" => arg(a).to_uppercase(),
            "camelCase" => {
                let w = words(&arg(a));
                w.iter().enumerate().map(|(i, w)| if i == 0 { w.clone() } else { capitalize(w) }).collect()
            }
            "snakeCase" => words(&arg(a)).join("_"),
            "spaceSeparated" => words(&arg(a)).join(" "),
            _ => String::new(),
        },
    }
}

/// Evaluate a variable expression (`""` for an empty or malformed one). `vars` are the values
/// of the variables before it.
pub fn eval_expression(src: &str, ctx: &ExpandContext, vars: &BTreeMap<String, String>) -> String {
    if src.trim().is_empty() {
        return String::new();
    }
    match parse_expr(src) {
        Ok(e) => eval(&e, ctx, vars).chars().take(10_000).collect(),
        Err(_) => String::new(),
    }
}

/// Most chars an expansion may produce (guards against absurd user templates or selections).
pub const MAX_EXPANSION_CHARS: usize = 1_000_000;

struct Writer<'a> {
    ctx: &'a ExpandContext,
    out: String,
    len: usize,
    at_line_start: bool,
    /// Whitespace at the start of the current output line.
    line_indent: String,
}

impl Writer<'_> {
    fn push_char(&mut self, c: char) {
        if self.len >= MAX_EXPANSION_CHARS {
            return;
        }
        match c {
            '\n' => {
                self.out.push('\n');
                self.out.push_str(&self.ctx.indent);
                self.len += 1 + self.ctx.indent.chars().count();
                self.line_indent = self.ctx.indent.clone();
                self.at_line_start = true;
            }
            '\t' if self.at_line_start => {
                self.out.push_str(&self.ctx.indent_unit);
                self.len += self.ctx.indent_unit.chars().count();
                self.line_indent.push_str(&self.ctx.indent_unit);
            }
            ' ' if self.at_line_start => {
                self.out.push(' ');
                self.len += 1;
                self.line_indent.push(' ');
            }
            c => {
                self.out.push(c);
                self.len += 1;
                self.at_line_start = false;
            }
        }
    }

    /// Insert a value verbatim except that its later lines get the current line's indentation.
    fn push_value(&mut self, value: &str) {
        let indent = self.line_indent.clone();
        for (i, line) in value.split('\n').enumerate() {
            if i > 0 {
                self.out.push('\n');
                self.len += 1;
                if !line.trim().is_empty() {
                    self.out.push_str(&indent);
                    self.len += indent.chars().count();
                }
            }
            for c in line.chars() {
                if self.len >= MAX_EXPANSION_CHARS {
                    return;
                }
                self.out.push(c);
                self.len += 1;
            }
            if !line.is_empty() {
                self.at_line_start = false;
            }
        }
    }
}

/// Expand a template.
pub fn expand(t: &Template, ctx: &ExpandContext) -> Expansion {
    let mut w = Writer { ctx, out: String::new(), len: 0, at_line_start: true, line_indent: ctx.indent.clone() };
    let mut names: Vec<String> = Vec::new();
    let mut vars: Vec<Vec<Range<usize>>> = Vec::new();
    let mut skip: Vec<bool> = Vec::new();
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    let mut end = None;
    for seg in segments(&t.text) {
        match seg {
            Segment::Text(s) => s.chars().for_each(|c| w.push_char(c)),
            Segment::Var(name) => {
                let value = match values.get(&name) {
                    Some(v) => v.clone(),
                    None => {
                        let expr = t.expressions.get(&name).map(|e| eval_expression(e, ctx, &values)).unwrap_or_default();
                        let v = if expr.is_empty() { t.defaults.get(&name).cloned().unwrap_or_default() } else { expr };
                        values.insert(name.clone(), v.clone());
                        v
                    }
                };
                let start = w.len;
                w.push_value(&value);
                let idx = match names.iter().position(|n| *n == name) {
                    Some(i) => i,
                    None => {
                        skip.push(!value.is_empty() && t.skip_if_defined.contains(&name));
                        names.push(name);
                        vars.push(Vec::new());
                        vars.len() - 1
                    }
                };
                if let Some(v) = vars.get_mut(idx) {
                    v.push(start..w.len);
                }
            }
            Segment::End => end = Some(w.len),
            Segment::Selection => {
                let sel = ctx.selection.clone().unwrap_or_default();
                w.push_value(&sel);
            }
        }
    }
    Expansion { text: w.out, vars, end, skip }
}

/// Remove the common leading whitespace of the non-blank lines (for `$SELECTION$`), and a
/// trailing newline.
pub fn dedent(text: &str) -> String {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let common = text
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').count())
        .min()
        .unwrap_or(0);
    text.split('\n').map(|l| l.chars().skip(common).collect::<String>()).collect::<Vec<_>>().join("\n")
}

// ------------------------------------------------------------------------------- built-ins

const RUST: &[&str] = &["rust"];
const PY: &[&str] = &["python"];
const TS_JS: &[&str] = &["typescript", "javascript"];
const C_CPP: &[&str] = &["c", "cpp"];
const CPP: &[&str] = &["cpp"];
const ZIG: &[&str] = &["zig"];
const SQL: &[&str] = &["sql"];

/// CodeCraft's built-in templates.
pub fn builtins() -> Vec<Template> {
    let t = Template::new;
    vec![
        // Rust
        t("fn", "Function", "fn $NAME$($PARAMS$) {\n\t$END$\n}", RUST),
        t("pfn", "Public function", "pub fn $NAME$($PARAMS$) {\n\t$END$\n}", RUST),
        t("main", "main function", "fn main() {\n\t$END$\n}", RUST),
        t("test", "Test function", "#[test]\nfn $NAME$() {\n\t$END$\n}", RUST),
        t("tfn", "Test function", "#[test]\nfn $NAME$() {\n\t$END$\n}", RUST),
        t(
            "tmod",
            "Test module",
            "#[cfg(test)]\nmod tests {\n\tuse super::*;\n\n\t#[test]\n\tfn $NAME$() {\n\t\t$END$\n\t}\n}",
            RUST,
        ),
        t("st", "Struct", "struct $NAME$ {\n\t$END$\n}", RUST),
        t("pst", "Public struct", "pub struct $NAME$ {\n\t$END$\n}", RUST),
        t("en", "Enum", "enum $NAME$ {\n\t$END$\n}", RUST),
        t("tr", "Trait", "trait $NAME$ {\n\t$END$\n}", RUST),
        t("impl", "Impl block", "impl $TYPE$ {\n\t$END$\n}", RUST),
        t("implt", "Trait impl", "impl $TRAIT$ for $TYPE$ {\n\t$END$\n}", RUST),
        t("mod", "Module", "mod $NAME$ {\n\t$END$\n}", RUST),
        t("p", "println!", "println!(\"$END$\");", RUST),
        t("pv", "Print a value", "println!(\"$EXPR$ = {:?}\", $EXPR$);$END$", RUST),
        t("ep", "eprintln!", "eprintln!(\"$END$\");", RUST),
        t("d", "dbg!", "dbg!($EXPR$);$END$", RUST),
        t("if", "if", "if $COND$ {\n\t$END$\n}", RUST),
        t("ife", "if … else", "if $COND$ {\n\t$END$\n} else {\n\t\n}", RUST),
        t("ifl", "if let", "if let $PAT$ = $EXPR$ {\n\t$END$\n}", RUST).with_default("PAT", "Some(x)"),
        t("whl", "while let", "while let $PAT$ = $EXPR$ {\n\t$END$\n}", RUST).with_default("PAT", "Some(x)"),
        t("matchop", "match on Option", "match $EXPR$ {\n\tSome($VAL$) => $END$,\n\tNone => {}\n}", RUST)
            .with_default("VAL", "v"),
        t("matchres", "match on Result", "match $EXPR$ {\n\tOk($VAL$) => $END$,\n\tErr($ERR$) => {}\n}", RUST)
            .with_default("VAL", "v")
            .with_default("ERR", "e"),
        t("for", "for loop", "for $VAR$ in $ITER$ {\n\t$END$\n}", RUST).with_default("VAR", "item"),
        t("fori", "for over a range", "for $I$ in 0..$N$ {\n\t$END$\n}", RUST).with_default("I", "i"),
        t("while", "while loop", "while $COND$ {\n\t$END$\n}", RUST),
        t("loop", "Infinite loop", "loop {\n\t$END$\n}", RUST),
        t("let", "let binding", "let $NAME$ = $END$;", RUST),
        t("letm", "let mut binding", "let mut $NAME$ = $END$;", RUST),
        t("der", "derive attribute", "#[derive($END$)]", RUST),
        t("macro", "macro_rules!", "macro_rules! $NAME$ {\n\t($ARGS$) => {\n\t\t$END$\n\t};\n}", RUST),
        // Rust selection templates (Surround With)
        t("sif", "if", "if $COND$ {\n\t$SELECTION$\n}$END$", RUST),
        t("sifl", "if let", "if let $PAT$ = $EXPR$ {\n\t$SELECTION$\n}$END$", RUST).with_default("PAT", "Some(x)"),
        t("swhile", "while", "while $COND$ {\n\t$SELECTION$\n}$END$", RUST),
        t("sfor", "for", "for $VAR$ in $ITER$ {\n\t$SELECTION$\n}$END$", RUST).with_default("VAR", "item"),
        t("sloop", "loop", "loop {\n\t$SELECTION$\n}$END$", RUST),
        t("sblock", "{ } block", "{\n\t$SELECTION$\n}$END$", RUST),
        t("sunsafe", "unsafe block", "unsafe {\n\t$SELECTION$\n}$END$", RUST),
        t("sclosure", "Closure", "|$ARGS$| {\n\t$SELECTION$\n}$END$", RUST),
        t("sdbg", "dbg!(…)", "dbg!($SELECTION$)$END$", RUST),
        t("ssome", "Some(…)", "Some($SELECTION$)$END$", RUST),
        t("sok", "Ok(…)", "Ok($SELECTION$)$END$", RUST),
        t("sparen", "( )", "($SELECTION$)$END$", RUST),
        // Python
        t("main", "if __name__ == \"__main__\"", "if __name__ == \"__main__\":\n\t$END$", PY),
        t("def", "Function", "def $NAME$($PARAMS$):\n\t$BODY$", PY).with_default("BODY", "pass"),
        t("class", "Class", "class $NAME$:\n\tdef __init__(self$PARAMS$):\n\t\t$END$", PY),
        t("if", "if", "if $COND$:\n\t$END$", PY),
        t("for", "for loop", "for $VAR$ in $ITER$:\n\t$END$", PY).with_default("VAR", "item"),
        t("while", "while loop", "while $COND$:\n\t$END$", PY),
        t("with", "with statement", "with $EXPR$ as $NAME$:\n\t$END$", PY),
        t("try", "try/except", "try:\n\t$END$\nexcept $EXC$ as e:\n\traise", PY).with_default("EXC", "Exception"),
        t("p", "print()", "print($END$)", PY),
        t("sif", "if", "if $COND$:\n\t$SELECTION$\n$END$", PY),
        t("sfor", "for", "for $VAR$ in $ITER$:\n\t$SELECTION$\n$END$", PY).with_default("VAR", "item"),
        t("swhile", "while", "while $COND$:\n\t$SELECTION$\n$END$", PY),
        t("stry", "try/except", "try:\n\t$SELECTION$\nexcept $EXC$ as e:\n\traise$END$", PY)
            .with_default("EXC", "Exception"),
        t("swith", "with", "with $EXPR$ as $NAME$:\n\t$SELECTION$\n$END$", PY),
        // TypeScript / JavaScript
        t("fn", "Function", "function $NAME$($PARAMS$) {\n\t$END$\n}", TS_JS),
        t("afn", "Arrow function", "const $NAME$ = ($PARAMS$) => {\n\t$END$\n};", TS_JS),
        t("log", "console.log", "console.log($END$);", TS_JS),
        t("if", "if", "if ($COND$) {\n\t$END$\n}", TS_JS),
        t("for", "for loop", "for (let $I$ = 0; $I$ < $N$; $I$++) {\n\t$END$\n}", TS_JS).with_default("I", "i"),
        t("forof", "for … of", "for (const $ITEM$ of $ITER$) {\n\t$END$\n}", TS_JS).with_default("ITEM", "item"),
        t("while", "while loop", "while ($COND$) {\n\t$END$\n}", TS_JS),
        t("cls", "Class", "class $NAME$ {\n\tconstructor($PARAMS$) {\n\t\t$END$\n\t}\n}", TS_JS),
        t("imp", "import", "import { $NAME$ } from \"$MODULE$\";$END$", TS_JS),
        t("test", "Test case", "test(\"$NAME$\", () => {\n\t$END$\n});", TS_JS),
        t("sif", "if", "if ($COND$) {\n\t$SELECTION$\n}$END$", TS_JS),
        t("stry", "try/catch", "try {\n\t$SELECTION$\n} catch (e) {\n\t$END$\n}", TS_JS),
        t("sblock", "{ } block", "{\n\t$SELECTION$\n}$END$", TS_JS),
        // C / C++
        t("main", "main function", "int main(int argc, char *argv[]) {\n\t$END$\n\treturn 0;\n}", C_CPP),
        t("if", "if", "if ($COND$) {\n\t$END$\n}", C_CPP),
        t("for", "for loop", "for (int $I$ = 0; $I$ < $N$; $I$++) {\n\t$END$\n}", C_CPP).with_default("I", "i"),
        t("while", "while loop", "while ($COND$) {\n\t$END$\n}", C_CPP),
        t("pf", "printf", "printf(\"$END$\\n\");", C_CPP),
        t("st", "struct", "struct $NAME$ {\n\t$END$\n};", C_CPP),
        t("inc", "#include", "#include <$END$>", C_CPP),
        t("cls", "Class", "class $NAME$ {\npublic:\n\t$END$\n};", CPP),
        t("ns", "Namespace", "namespace $NAME$ {\n\n$END$\n\n}", CPP),
        t("sif", "if", "if ($COND$) {\n\t$SELECTION$\n}$END$", C_CPP),
        t("sblock", "{ } block", "{\n\t$SELECTION$\n}$END$", C_CPP),
        t("swhile", "while", "while ($COND$) {\n\t$SELECTION$\n}$END$", C_CPP),
        // Zig
        t("fn", "Function", "fn $NAME$($PARAMS$) void {\n\t$END$\n}", ZIG),
        t("pfn", "Public function", "pub fn $NAME$($PARAMS$) void {\n\t$END$\n}", ZIG),
        t("main", "main function", "pub fn main() !void {\n\t$END$\n}", ZIG),
        t("test", "Test block", "test \"$NAME$\" {\n\t$END$\n}", ZIG),
        t("if", "if", "if ($COND$) {\n\t$END$\n}", ZIG),
        t("for", "for loop", "for ($ITEMS$) |$ITEM$| {\n\t$END$\n}", ZIG).with_default("ITEM", "item"),
        t("while", "while loop", "while ($COND$) {\n\t$END$\n}", ZIG),
        t("p", "std.debug.print", "std.debug.print(\"$END$\\n\", .{});", ZIG),
        t("st", "struct", "const $NAME$ = struct {\n\t$END$\n};", ZIG),
        t("sif", "if", "if ($COND$) {\n\t$SELECTION$\n}$END$", ZIG),
        t("sblock", "{ } block", "{\n\t$SELECTION$\n}$END$", ZIG),
        // SQL (track DC)
        t("sel", "SELECT … FROM", "SELECT $COLUMNS$\nFROM $TABLE$$END$;", SQL).with_default("COLUMNS", "*"),
        t("selw", "SELECT … FROM … WHERE", "SELECT $COLUMNS$\nFROM $TABLE$\nWHERE $CONDITION$$END$;", SQL)
            .with_default("COLUMNS", "*"),
        t("ins", "INSERT INTO … VALUES", "INSERT INTO $TABLE$ ($COLUMNS$)\nVALUES ($VALUES$)$END$;", SQL),
        t("upd", "UPDATE … SET … WHERE", "UPDATE $TABLE$\nSET $COLUMN$ = $VALUE$\nWHERE $CONDITION$$END$;", SQL),
        t("del", "DELETE FROM … WHERE", "DELETE\nFROM $TABLE$\nWHERE $CONDITION$$END$;", SQL),
        t("j", "JOIN … ON", "JOIN $TABLE$ $ALIAS$ ON $CONDITION$$END$", SQL),
        t("lj", "LEFT JOIN … ON", "LEFT JOIN $TABLE$ $ALIAS$ ON $CONDITION$$END$", SQL),
        t("cte", "WITH … AS (…)", "WITH $NAME$ AS (\n\t$QUERY$\n)\nSELECT *\nFROM $NAME$$END$;", SQL)
            .with_default("NAME", "cte"),
        t("sgb", "SELECT … GROUP BY", "SELECT $COLUMN$, count(*)\nFROM $TABLE$\nGROUP BY $COLUMN$$END$;", SQL),
    ]
}

// ------------------------------------------------------------------------------- user store

/// File format of `templates.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TemplatesFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    templates: Vec<Template>,
}

/// Built-in plus user templates; user templates load lazily from `templates.json`.
#[derive(Debug, Default)]
pub struct TemplateStore {
    user: Vec<Template>,
    loaded: bool,
    /// Explicit file (tests and embedders); otherwise `$CODECRAFT_HOME` or `~/.codecraft`.
    path_override: Option<PathBuf>,
    /// The last load error (a corrupt file is set aside on the next save, never overwritten).
    pub error: Option<String>,
    corrupt: bool,
}

impl TemplateStore {
    /// Use this file for user templates (reloads on next use).
    pub fn set_path(&mut self, path: PathBuf) {
        self.path_override = Some(path);
        self.loaded = false;
        self.user.clear();
        self.error = None;
        self.corrupt = false;
    }

    /// Where user templates are stored.
    pub fn path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.path_override {
            return Some(p.clone());
        }
        if let Some(home) = std::env::var_os("CODECRAFT_HOME").filter(|h| !h.is_empty()) {
            return Some(PathBuf::from(home).join("templates.json"));
        }
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).filter(|h| !h.is_empty())?;
        Some(PathBuf::from(home).join(".codecraft").join("templates.json"))
    }

    fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let Some(path) = self.path() else { return };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                self.error = Some(format!("cannot read {}: {e}", path.display()));
                return;
            }
        };
        match serde_json::from_str::<TemplatesFile>(&text) {
            Ok(f) => {
                self.user = f.templates.into_iter().filter(|t| !t.abbreviation.trim().is_empty()).collect();
            }
            Err(e) => {
                self.error = Some(format!("ignoring corrupt {}: {e}", path.display()));
                self.corrupt = true;
            }
        }
    }

    /// User templates only.
    pub fn user_templates(&mut self) -> &[Template] {
        self.ensure_loaded();
        &self.user
    }

    /// Every enabled template for a language: user templates first, then built-ins that no user
    /// template (enabled or not) with the same abbreviation overrides.
    pub fn for_language(&mut self, language: Language) -> Vec<Template> {
        self.ensure_loaded();
        let mine: Vec<&Template> = self.user.iter().filter(|t| t.applies_to(language)).collect();
        let mut out: Vec<Template> = mine.iter().filter(|t| t.enabled).map(|t| (*t).clone()).collect();
        for b in builtins().into_iter().filter(|t| t.applies_to(language)) {
            if !mine.iter().any(|u| u.abbreviation == b.abbreviation) {
                out.push(b);
            }
        }
        out
    }

    /// Every template as Settings › Live Templates lists it: user templates (with whether they
    /// replace a built-in), then the built-ins none replaces.
    pub fn entries(&mut self) -> Vec<TemplateEntry> {
        self.ensure_loaded();
        let all = builtins();
        let mut out: Vec<TemplateEntry> = self
            .user
            .iter()
            .map(|t| TemplateEntry { template: t.clone(), builtin: false, replaces_builtin: all.iter().any(|b| t.overlaps(b)) })
            .collect();
        for b in all {
            if !self.user.iter().any(|u| u.overlaps(&b)) {
                out.push(TemplateEntry { template: b, builtin: true, replaces_builtin: false });
            }
        }
        out
    }

    fn write(&mut self) -> Result<PathBuf, String> {
        let path = self.path().ok_or("cannot find the home directory; set CODECRAFT_HOME")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        if self.corrupt {
            let mut bad = path.clone().into_os_string();
            bad.push(".bad");
            let _ = std::fs::rename(&path, PathBuf::from(bad));
            self.corrupt = false;
        }
        let file = TemplatesFile { version: 1, templates: self.user.clone() };
        let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
        codecraft_fs::write_text_file_atomic(&path, &text).map_err(|e| format!("cannot save {}: {e}", path.display()))?;
        self.error = None;
        Ok(path)
    }

    /// Save a user template (validated), replacing `original` (the template being edited:
    /// abbreviation and languages) and any user template it overlaps. Returns the file path.
    pub fn save_template(&mut self, t: Template, original: Option<&Template>) -> Result<PathBuf, String> {
        let t = validate(t)?;
        self.ensure_loaded();
        let before = self.user.clone();
        if let Some(o) = original {
            self.user.retain(|u| !(u.abbreviation == o.abbreviation && u.languages == o.languages));
        }
        self.user.retain(|u| !u.overlaps(&t));
        self.user.push(t);
        self.write().inspect_err(|_| self.user = before)
    }

    /// Delete a template: a user template is removed (a built-in it replaced comes back); a
    /// built-in is hidden by a disabled copy (Restore Defaults brings it back).
    pub fn delete(&mut self, abbreviation: &str, language: Option<&str>) -> Result<PathBuf, String> {
        self.ensure_loaded();
        let matches = |t: &Template| t.abbreviation == abbreviation && language.is_none_or(|l| t.languages.iter().any(|x| x == l));
        let before = self.user.clone();
        if self.user.iter().any(&matches) {
            self.user.retain(|u| !matches(u));
        } else {
            let found: Vec<Template> = builtins().into_iter().filter(|b| matches(b)).collect();
            if found.is_empty() {
                return Err(format!("no live template `{abbreviation}`"));
            }
            for mut b in found {
                b.enabled = false;
                self.user.push(b);
            }
        }
        self.write().inspect_err(|_| self.user = before)
    }

    /// Turn a template on or off (a built-in gets a user copy with the flag).
    pub fn set_enabled(&mut self, abbreviation: &str, language: Option<&str>, enabled: bool) -> Result<PathBuf, String> {
        self.ensure_loaded();
        let matches = |t: &Template| t.abbreviation == abbreviation && language.is_none_or(|l| t.languages.iter().any(|x| x == l));
        let before = self.user.clone();
        let mut hit = false;
        for u in self.user.iter_mut().filter(|u| matches(u)) {
            u.enabled = enabled;
            hit = true;
        }
        if !hit {
            let found: Vec<Template> = builtins().into_iter().filter(|b| matches(b)).collect();
            if found.is_empty() {
                return Err(format!("no live template `{abbreviation}`"));
            }
            for mut b in found {
                b.enabled = enabled;
                self.user.push(b);
            }
        }
        self.write().inspect_err(|_| self.user = before)
    }

    /// Settings › Live Templates › Restore Defaults: drop the user's changes to built-ins
    /// (edited, disabled or deleted ones); the user's own templates stay.
    pub fn restore_defaults(&mut self) -> Result<PathBuf, String> {
        self.ensure_loaded();
        let all = builtins();
        let before = self.user.clone();
        self.user.retain(|u| !all.iter().any(|b| u.overlaps(b)));
        self.write().inspect_err(|_| self.user = before)
    }

    /// The template with this abbreviation for a language.
    pub fn find(&mut self, language: Language, abbreviation: &str) -> Option<Template> {
        self.for_language(language).into_iter().find(|t| t.abbreviation == abbreviation)
    }

    /// Add (or replace) a user template and save the file. Returns the file path.
    pub fn add_user(&mut self, t: Template) -> Result<PathBuf, String> {
        self.ensure_loaded();
        self.user.retain(|u| !(u.abbreviation == t.abbreviation && u.languages.iter().any(|l| t.languages.contains(l))));
        self.user.push(t);
        self.write()
    }
}

/// One row of Settings › Live Templates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateEntry {
    pub template: Template,
    /// CodeCraft's own template (not changed by the user).
    pub builtin: bool,
    /// A user template that replaces (or hides) a built-in.
    pub replaces_builtin: bool,
}

/// Language ids a template may apply to (contexts in Settings › Live Templates).
pub const CONTEXTS: &[&str] = &[
    "any",
    "rust",
    "toml",
    "python",
    "typescript",
    "javascript",
    "c",
    "cpp",
    "zig",
    "sql",
    "markdown",
    "json",
    "yaml",
    "shellscript",
    "django-html",
    "http",
    "dockerfile",
    "plaintext",
];

/// Most characters of template text.
pub const MAX_TEMPLATE_TEXT: usize = 100_000;

/// Validate a template for saving: a word abbreviation (≤ 64), text, at least one known
/// context, variable names, expressions that parse and defaults without newlines' worth of
/// garbage. Returns it trimmed.
pub fn validate(mut t: Template) -> Result<Template, String> {
    t.abbreviation = t.abbreviation.trim().to_string();
    if t.abbreviation.is_empty()
        || t.abbreviation.chars().count() > 64
        || !t.abbreviation.chars().all(|c| c.is_alphanumeric() || c == '_')
    {
        return Err("the abbreviation must be a word (letters, digits, `_`) of at most 64 characters".into());
    }
    if t.text.trim().is_empty() {
        return Err("the template text is empty".into());
    }
    if t.text.chars().count() > MAX_TEMPLATE_TEXT {
        return Err("the template text is too long".into());
    }
    t.description = t.description.trim().chars().take(200).collect();
    let mut langs: Vec<String> = Vec::new();
    for l in &t.languages {
        let l = l.trim().to_ascii_lowercase();
        if !CONTEXTS.contains(&l.as_str()) {
            return Err(format!("unknown context `{l}`; use {}", CONTEXTS.join(", ")));
        }
        if !langs.contains(&l) {
            langs.push(l);
        }
    }
    if langs.is_empty() {
        return Err("choose at least one context (language) for the template".into());
    }
    t.languages = langs;
    let names = t.variables();
    for (name, e) in &t.expressions {
        if !names.contains(name) {
            return Err(format!("`{name}` is not a variable of the template"));
        }
        check_expression(e).map_err(|err| format!("{name}: {err}"))?;
    }
    t.expressions.retain(|_, e| !e.trim().is_empty());
    for name in t.defaults.keys().chain(t.skip_if_defined.iter()) {
        if !names.contains(name) {
            return Err(format!("`{name}` is not a variable of the template"));
        }
    }
    t.defaults.retain(|_, v| !v.is_empty());
    if t.defaults.values().any(|v| v.chars().count() > 1000) {
        return Err("a default value is too long".into());
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(indent: &str) -> ExpandContext {
        ExpandContext { indent: indent.into(), indent_unit: "    ".into(), selection: None, ..Default::default() }
    }

    #[test]
    fn expressions_compute_values_and_skip() {
        let mut t = Template::new("st", "", "struct $NAME$ { $FIELD$: u32 } // $WHEN$ $NAME$ $$", &["rust"]);
        t.expressions.insert("NAME".into(), "capitalize(camelCase(fileNameWithoutExtension()))".into());
        t.expressions.insert("FIELD".into(), "snakeCase(NAME)".into());
        t.expressions.insert("WHEN".into(), "date()".into());
        t.skip_if_defined.push("NAME".into());
        let c = ExpandContext { file_name: Some("my_point.rs".into()), now: Some(1_700_000_000), ..ctx("") };
        let e = expand(&t, &c);
        assert_eq!(e.text, "struct MyPoint { my_point: u32 } // 2023-11-14 MyPoint $");
        assert_eq!(e.skip, vec![true, false, false]);
        // An empty result falls back to the default.
        let mut d = Template::new("x", "", "$A$", &["any"]).with_default("A", "dflt");
        d.expressions.insert("A".into(), "fileName()".into());
        assert_eq!(expand(&d, &ctx("")).text, "dflt");
        assert_eq!(eval_expression("\"lit\"", &ctx(""), &BTreeMap::new()), "lit");
        assert_eq!(eval_expression("lineNumber()", &ExpandContext { line: 4, ..ctx("") }, &BTreeMap::new()), "5");
        assert_eq!(eval_expression("time()", &ExpandContext { now: Some(3_723), ..ctx("") }, &BTreeMap::new()), "01:02");
        assert_eq!(eval_expression("uppercase(spaceSeparated(\"fooBar_baz\"))", &ctx(""), &BTreeMap::new()), "FOO BAR BAZ");
        for bad in ["nope()", "capitalize(", "\"open", "a b", ")", &"capitalize(".repeat(40)] {
            assert!(check_expression(bad).is_err(), "{bad}");
            assert_eq!(eval_expression(bad, &ctx(""), &BTreeMap::new()), "");
        }
        assert!(check_expression("").is_ok());
    }

    #[test]
    fn editing_user_and_builtin_templates() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = TemplateStore::default();
        store.set_path(dir.path().join("templates.json"));
        let n = store.entries().len();
        assert_eq!(n, builtins().len());
        // A new template, then edited under a new abbreviation.
        let t = Template::new("hello", "Hi", "println!(\"hi $NAME$\");", &["rust"]);
        store.save_template(t.clone(), None).unwrap();
        let mut edited = t.clone();
        edited.abbreviation = "hey".into();
        edited.languages = vec!["rust".into(), "python".into()];
        store.save_template(edited, Some(&t)).unwrap();
        assert!(store.find(Language::Rust, "hello").is_none());
        assert!(store.find(Language::Python, "hey").is_some());
        // Editing a built-in replaces it; deleting the copy brings it back.
        let mut p = builtins().into_iter().find(|b| b.abbreviation == "p" && b.languages == ["rust"]).unwrap();
        p.text = "print!(\"$END$\");".into();
        store.save_template(p, None).unwrap();
        assert_eq!(store.find(Language::Rust, "p").unwrap().text, "print!(\"$END$\");");
        assert!(store.entries().iter().any(|e| e.template.abbreviation == "p" && e.replaces_builtin));
        store.delete("p", Some("rust")).unwrap();
        assert_eq!(store.find(Language::Rust, "p").unwrap().text, "println!(\"$END$\");");
        // Deleting a built-in hides it until Restore Defaults.
        store.delete("fn", Some("rust")).unwrap();
        assert!(store.find(Language::Rust, "fn").is_none());
        assert!(store.find(Language::Zig, "fn").is_some(), "other languages keep theirs");
        store.set_enabled("main", Some("rust"), false).unwrap();
        assert!(store.find(Language::Rust, "main").is_none());
        store.restore_defaults().unwrap();
        assert!(store.find(Language::Rust, "fn").is_some() && store.find(Language::Rust, "main").is_some());
        assert!(store.find(Language::Python, "hey").is_some(), "the user's own templates stay");
        // Saved in the file.
        let mut again = TemplateStore::default();
        again.set_path(dir.path().join("templates.json"));
        assert!(again.find(Language::Rust, "hey").is_some());
        assert!(store.delete("nothing", None).is_err());
    }

    #[test]
    fn validation_rejects_bad_templates() {
        let ok = Template::new("a_1", "", "x $V$", &["rust"]);
        assert!(validate(ok.clone()).is_ok());
        let mut bad_expr = ok.clone();
        bad_expr.expressions.insert("V".into(), "nope()".into());
        let mut bad_var = ok.clone();
        bad_var.expressions.insert("W".into(), "date()".into());
        let mut bad_default = ok.clone();
        bad_default.defaults.insert("W".into(), "x".into());
        for t in [
            Template::new("", "", "x", &["rust"]),
            Template::new("a b", "", "x", &["rust"]),
            Template::new("a", "", "  ", &["rust"]),
            Template::new("a", "", "x", &[]),
            Template::new("a", "", "x", &["cobol"]),
            bad_expr,
            bad_var,
            bad_default,
        ] {
            assert!(validate(t.clone()).is_err(), "{t:?}");
        }
    }

    #[test]
    fn parses_placeholders_and_literal_dollars() {
        let t = Template::new("x", "", "a $A$ $$ b $B$ $A$ $END$ $5 $ $-$", &["any"]);
        assert_eq!(t.variables(), vec!["A".to_string(), "B".to_string()]);
        let e = expand(&t, &ctx(""));
        assert_eq!(e.text, "a  $ b    $5 $ $-$");
        assert_eq!(e.vars, vec![vec![2..2, 8..8], vec![7..7]]);
        assert_eq!(e.end, Some(9));
    }

    #[test]
    fn indents_lines_and_uses_defaults() {
        let t = Template::new("for", "", "for $VAR$ in $ITER$ {\n\t$END$\n}", &["rust"]).with_default("VAR", "item");
        let e = expand(&t, &ctx("  "));
        assert_eq!(e.text, "for item in  {\n      \n  }");
        assert_eq!(e.vars, vec![vec![4..8], vec![12..12]]);
        assert_eq!(e.end, Some(21));
    }

    #[test]
    fn selection_is_reindented() {
        let t = Template::new("sif", "", "if $COND$ {\n\t$SELECTION$\n}$END$", &["rust"]);
        let mut c = ctx("    ");
        c.selection = Some(dedent("        a();\n        if x {\n            b();\n        }\n"));
        let e = expand(&t, &c);
        assert_eq!(e.text, "if  {\n        a();\n        if x {\n            b();\n        }\n    }");
        assert!(t.is_surround());
    }

    #[test]
    fn dedent_keeps_relative_indentation() {
        assert_eq!(dedent("  a\n\n    b\n"), "a\n\n  b");
        assert_eq!(dedent(""), "");
    }

    #[test]
    fn builtins_are_well_formed() {
        let all = builtins();
        assert!(all.len() > 60);
        let langs = [Language::Rust, Language::Python, Language::TypeScript, Language::C, Language::Zig, Language::Sql];
        for lang in langs {
            let mut seen = std::collections::HashSet::new();
            for t in all.iter().filter(|t| t.applies_to(lang)) {
                assert!(seen.insert(t.abbreviation.clone()), "duplicate {} for {lang:?}", t.abbreviation);
                assert!(!t.description.is_empty());
                let e = expand(t, &ctx(""));
                assert!(!e.text.is_empty(), "{}", t.abbreviation);
                for v in e.vars.iter().flatten() {
                    assert!(v.end <= e.text.chars().count());
                }
            }
        }
        assert!(all.iter().any(|t| t.abbreviation == "sif" && t.is_surround()));
        assert!(Template::new("x", "", "", &["cpp"]).applies_to(Language::Cpp));
        assert!(!Template::new("x", "", "", &["cpp"]).applies_to(Language::Rust));
    }

    #[test]
    fn user_templates_persist_and_override_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/templates.json");
        let mut store = TemplateStore::default();
        store.set_path(path.clone());
        assert!(store.user_templates().is_empty());
        let saved = store.add_user(Template::new("p", "Mine", "print!(\"$END$\");", &["rust"])).unwrap();
        assert_eq!(saved, path);
        let mut again = TemplateStore::default();
        again.set_path(path.clone());
        assert_eq!(again.find(Language::Rust, "p").unwrap().description, "Mine");
        assert_eq!(again.for_language(Language::Rust).iter().filter(|t| t.abbreviation == "p").count(), 1);
        // Replacing keeps one entry.
        again.add_user(Template::new("p", "Mine 2", "x", &["rust"])).unwrap();
        assert_eq!(again.user_templates().len(), 1);
    }

    #[test]
    fn corrupt_file_is_reported_and_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("templates.json");
        std::fs::write(&path, "{ not json").unwrap();
        let mut store = TemplateStore::default();
        store.set_path(path.clone());
        assert!(store.user_templates().is_empty());
        assert!(store.error.as_deref().unwrap_or("").contains("corrupt"));
        assert!(store.find(Language::Rust, "fn").is_some(), "built-ins still work");
        store.add_user(Template::new("q", "Q", "q", &["rust"])).unwrap();
        assert!(dir.path().join("templates.json.bad").exists());
    }
}
