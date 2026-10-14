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
}

impl Template {
    pub fn new(abbreviation: &str, description: &str, text: &str, languages: &[&str]) -> Self {
        Self {
            abbreviation: abbreviation.to_string(),
            description: description.to_string(),
            text: text.to_string(),
            languages: languages.iter().map(|l| (*l).to_string()).collect(),
            defaults: BTreeMap::new(),
        }
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
}

/// An expanded template: text to insert and placeholder positions (char offsets in `text`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expansion {
    pub text: String,
    /// One entry per variable (in order), with every occurrence.
    pub vars: Vec<Vec<Range<usize>>>,
    pub end: Option<usize>,
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
    let mut end = None;
    for seg in segments(&t.text) {
        match seg {
            Segment::Text(s) => s.chars().for_each(|c| w.push_char(c)),
            Segment::Var(name) => {
                let value = t.defaults.get(&name).cloned().unwrap_or_default();
                let start = w.len;
                w.push_value(&value);
                let idx = match names.iter().position(|n| *n == name) {
                    Some(i) => i,
                    None => {
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
    Expansion { text: w.out, vars, end }
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

    /// Every template for a language: user templates first, then built-ins that no user
    /// template with the same abbreviation overrides.
    pub fn for_language(&mut self, language: Language) -> Vec<Template> {
        self.ensure_loaded();
        let mut out: Vec<Template> = self.user.iter().filter(|t| t.applies_to(language)).cloned().collect();
        for b in builtins().into_iter().filter(|t| t.applies_to(language)) {
            if !out.iter().any(|u| u.abbreviation == b.abbreviation) {
                out.push(b);
            }
        }
        out
    }

    /// The template with this abbreviation for a language.
    pub fn find(&mut self, language: Language, abbreviation: &str) -> Option<Template> {
        self.for_language(language).into_iter().find(|t| t.abbreviation == abbreviation)
    }

    /// Add (or replace) a user template and save the file. Returns the file path.
    pub fn add_user(&mut self, t: Template) -> Result<PathBuf, String> {
        self.ensure_loaded();
        let path = self.path().ok_or("cannot find the home directory; set CODECRAFT_HOME")?;
        self.user.retain(|u| !(u.abbreviation == t.abbreviation && u.languages.iter().any(|l| t.languages.contains(l))));
        self.user.push(t);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(indent: &str) -> ExpandContext {
        ExpandContext { indent: indent.into(), indent_unit: "    ".into(), selection: None }
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
