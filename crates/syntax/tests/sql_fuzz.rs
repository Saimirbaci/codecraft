//! SQL injection, formatting and name analysis on random input: never a panic, offsets in
//! range, and formatting is idempotent and changes only whitespace and keyword case. Deterministic xorshift input, so failures reproduce.

use codecraft_syntax::sql_format::{KeywordCase, SqlFormatOptions, format_sql};
use codecraft_syntax::sql_inject::{find_injections, injected_highlights};
use codecraft_syntax::sql_scope::{SqlColumn, SqlSchema, analyze, check, complete};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

const PIECES: &[&str] = &[
    "select ",
    "SELECT ",
    "from ",
    "FROM ",
    "where ",
    "and ",
    "or ",
    "between ",
    "join ",
    "left ",
    "on ",
    "group by ",
    "order by ",
    "insert into ",
    "values ",
    "update ",
    "set ",
    "delete ",
    "with ",
    " as ",
    "create table ",
    "union ",
    "(",
    ")",
    ",",
    ";",
    ".",
    "*",
    "=",
    "<>",
    "-",
    "+",
    "::",
    "'",
    "\"",
    "`",
    "$$",
    "$1",
    "?",
    ":name",
    "--",
    "/*",
    "*/",
    "\n",
    " ",
    "\t",
    "users",
    "u",
    "id",
    "name",
    "é",
    "😀",
    "\\",
    "\\n",
    "\\\"",
    "r#\"",
    "\"#",
    "query!(",
    "sqlx::query(",
    ".execute(",
    "// language=SQL\n",
    "{",
    "}",
    "[",
    "]",
    "!",
    "#",
    "1",
    "0.5",
    "case ",
    "when ",
    "end ",
    "\r\n",
];

fn random_text(rng: &mut Rng, len: usize) -> String {
    (0..len).map(|_| PIECES[rng.below(PIECES.len())]).collect()
}

struct Schema;

impl SqlSchema for Schema {
    fn tables(&self, schema: Option<&str>) -> Option<Vec<String>> {
        schema.is_none().then(|| vec!["users".to_string(), "orders".to_string()])
    }

    fn columns(&self, _: Option<&str>, table: &str) -> Option<Vec<SqlColumn>> {
        (table == "users").then(|| {
            ["id", "name"].iter().map(|c| SqlColumn { name: c.to_string(), data_type: "int".into() }).collect()
        })
    }

    fn schemas(&self) -> Vec<String> {
        vec!["main".into()]
    }
}

#[test]
fn random_sql_and_rust() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let options = [
        SqlFormatOptions::default(),
        SqlFormatOptions { keyword_case: KeywordCase::Lower, indent: 2 },
        SqlFormatOptions { keyword_case: KeywordCase::Preserve, indent: 1 },
    ];
    for round in 0..2000 {
        let len = 1 + rng.below(80);
        let text = random_text(&mut rng, len);
        let chars = text.chars().count();
        // Injection: content ranges inside the literal, inside the text.
        let found = find_injections(&text);
        for i in &found {
            assert!(i.literal_start <= i.start && i.start <= i.end && i.end <= i.literal_end && i.literal_end <= chars);
            for k in 0..=i.sql.chars().count() {
                let (a, b) = i.to_rust_range(0, k);
                assert!(a <= b && b <= i.end, "{text:?}");
            }
        }
        let hl = injected_highlights(&text, &found);
        assert_eq!(hl.len_lines(), text.split('\n').count());
        // Formatting: idempotent for every option set.
        let opts = &options[round % options.len()];
        let once = format_sql(&text, opts);
        let twice = format_sql(&once, opts);
        assert_eq!(once, twice, "not idempotent for {text:?}");
        // Only whitespace and keyword case change.
        let squeeze = |t: &str| t.to_lowercase().split_whitespace().collect::<String>();
        assert_eq!(squeeze(&once), squeeze(&text), "text changed for {text:?}");
        // Names.
        let _ = analyze(&text);
        for issue in check(&text, &Schema) {
            assert!(issue.start <= issue.end && issue.end <= chars);
        }
        let caret = rng.below(chars + 2);
        let c = complete(&text, caret, Some(&Schema));
        assert!(c.start <= caret.min(chars));
    }
}
