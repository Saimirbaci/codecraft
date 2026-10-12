//! Robustness and performance of the public API on hostile input.
//!
//! Every function must accept any text without panicking and return well-formed results.
//! Inputs come from a deterministic xorshift PRNG (no extra dependencies), so failures
//! reproduce exactly.

use std::time::{Duration, Instant};

use codecraft_syntax::{FoldRange, Highlights, Language, folding_ranges, highlight, matching_bracket, outline};

const LANGUAGES: [Language; 14] = [
    Language::Rust,
    Language::Toml,
    Language::Markdown,
    Language::Json,
    Language::Yaml,
    Language::Shell,
    Language::C,
    Language::Cpp,
    Language::Python,
    Language::Zig,
    Language::TypeScript,
    Language::JavaScript,
    Language::Sql,
    Language::PlainText,
];

/// xorshift64*: tiny, deterministic, good enough for fuzzing.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

/// Chars that drive the lexers into their interesting states.
const SOUP: &[char] = &[
    '(', ')', '[', ']', '{', '}', '<', '>', '"', '\'', '`', '#', '!', '*', '_', '/', '\\', '$', '-', '+', '=', ':',
    ';', ',', '.', '|', '&', '~', '?', '@', '%', '^', '\n', '\n', '\n', '\r', '\t', ' ', ' ', ' ', 'a', 'b', 'r', 'c',
    'x', 'E', 'O', 'F', 'i', 'f', 'n', '0', '1', '9', 'é', 'ß', '名', '😀', '\u{0}', '\u{200b}', '№',
];

/// Fragments that open and close multi-char constructs.
const FRAGMENTS: &[&str] = &[
    "/*",
    "*/",
    "//",
    "///",
    "//!",
    "r#\"",
    "\"#",
    "r##\"",
    "b'",
    "c\"",
    "'a",
    "#[",
    "#![",
    "]",
    "::<",
    "->",
    "=>",
    "macro_rules!",
    "fn ",
    "impl ",
    "use ",
    "pub ",
    "mod ",
    "trait ",
    "struct ",
    "enum ",
    "const ",
    "static ",
    "type ",
    "where ",
    "extern \"C\" ",
    "\"\"\"",
    "'''",
    "[[",
    "]]",
    "= ",
    "```",
    "~~~",
    "<!--",
    "-->",
    "# ",
    "## ",
    "> ",
    "- ",
    "1. ",
    "**",
    "__",
    "](",
    "<<EOF\n",
    "EOF\n",
    "<<-",
    "${",
    "$(",
    "if ",
    "then\n",
    "fi\n",
    "for ",
    " in ",
    "do\n",
    "done\n",
    "case ",
    "esac\n",
    "key: ",
    "- ",
    "|\n",
    ">-\n",
    "&a ",
    "*a ",
    "!!str ",
    "---\n",
    "{\"k\": ",
    "null",
    "true",
    "1e5",
    "0x",
    "\n    ",
    "\r\n",
    "😀",
    "é",
    // C/C++
    "#include <",
    "#define X ",
    "#if 0\n",
    "#endif\n",
    "R\"x(",
    ")x\"",
    "u8\"",
    "L'",
    "[[",
    "template<",
    "operator",
    "namespace ",
    "class ",
    "typedef ",
    "::",
    "->",
    // Python
    "def ",
    "    ",
    "f\"",
    "f'{",
    "rb'",
    "{x:",
    "@dec\n",
    "lambda ",
    "match ",
    "case ",
    "import ",
    "from ",
    ":\n",
    // Zig
    "\\\\",
    "@import(",
    "test \"",
    "pub fn ",
    "const X = struct {",
    "@\"",
    // TypeScript/JavaScript
    "`",
    "}",
    "/re/g",
    "<div>",
    "</div>",
    "/>",
    "<>",
    "=> ",
    "function ",
    "interface ",
    "export ",
    "#priv",
    // SQL
    "$$",
    "$tag$",
    "E'",
    "-- ",
    "BEGIN\n",
    "END;",
    "CASE ",
    "CREATE TABLE ",
    ":name",
    "U&'",
];

fn random_text(rng: &mut Rng) -> String {
    let len = rng.below(300);
    let mut s = String::new();
    match rng.below(3) {
        0 => {
            for _ in 0..len {
                s.push(rng.pick(SOUP));
            }
        }
        1 => {
            for _ in 0..len / 3 {
                if rng.below(2) == 0 {
                    s.push_str(rng.pick(FRAGMENTS));
                } else {
                    s.push(rng.pick(SOUP));
                }
            }
        }
        _ => {
            // Arbitrary bytes, decoded lossily (invalid sequences become U+FFFD).
            let bytes: Vec<u8> = (0..len).map(|_| (rng.next() >> 24) as u8).collect();
            s = String::from_utf8_lossy(&bytes).into_owned();
        }
    }
    s
}

fn line_lengths(text: &str) -> Vec<usize> {
    text.split('\n').map(|l| l.chars().count()).collect()
}

fn check_highlights(language: Language, text: &str, h: &Highlights) {
    let lens = line_lengths(text);
    assert_eq!(h.len_lines(), lens.len(), "{language:?} line count for {text:?}");
    for (i, &len) in lens.iter().enumerate() {
        let mut prev_end = 0;
        for s in h.line(i) {
            assert!(s.start < s.end && s.end <= len, "{language:?} bad span {s:?} on line {i} of {text:?}");
            assert!(s.start >= prev_end, "{language:?} overlapping spans on line {i} of {text:?}");
            prev_end = s.end;
        }
    }
    assert!(h.line(lens.len()).is_empty());
}

fn check_folds(language: Language, text: &str, folds: &[FoldRange]) {
    let lines = line_lengths(text).len();
    for f in folds {
        assert!(f.start_line < f.end_line && f.end_line < lines, "{language:?} bad fold {f:?} in {text:?}");
    }
    assert!(folds.windows(2).all(|w| w[0].start_line <= w[1].start_line), "{language:?} unsorted folds");
}

fn check_brackets(language: Language, text: &str, rng: &mut Rng) {
    let chars: Vec<char> = text.chars().collect();
    let pair = |a: char, b: char| {
        matches!((a, b), ('(', ')') | ('[', ']') | ('{', '}') | (')', '(') | (']', '[') | ('}', '{'))
    };
    let mut offsets = vec![0, chars.len(), chars.len() + 1, usize::MAX];
    offsets.extend((0..6).map(|_| rng.below(chars.len() + 2)));
    for offset in offsets {
        if let Some(partner) = matching_bracket(language, text, offset) {
            let at = [Some(offset), offset.checked_sub(1)]
                .into_iter()
                .flatten()
                .filter_map(|o| chars.get(o).copied())
                .collect::<Vec<_>>();
            let other = chars.get(partner).copied();
            assert!(
                other.is_some_and(|o| at.iter().any(|&a| pair(a, o))),
                "{language:?} bogus match {offset} -> {partner} in {text:?}"
            );
            // Matching is symmetric.
            let back = matching_bracket(language, text, partner);
            assert!(back.is_some(), "{language:?} asymmetric match at {partner} in {text:?}");
        }
    }
}

fn check_outline(language: Language, text: &str) {
    let lens = line_lengths(text);
    for item in outline(language, text) {
        let len = lens.get(item.line).copied();
        assert!(len.is_some_and(|l| item.column <= l), "{language:?} bad outline item {item:?} in {text:?}");
    }
}

fn exercise(language: Language, text: &str, rng: &mut Rng) {
    check_highlights(language, text, &highlight(language, text));
    check_folds(language, text, &folding_ranges(language, text));
    check_brackets(language, text, rng);
    check_outline(language, text);
}

/// Iterations for the randomized tests; set `SYNTAX_FUZZ_ITERS` for a deeper local run.
fn iterations(default: usize) -> usize {
    std::env::var("SYNTAX_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[test]
fn random_input_never_panics_and_is_well_formed() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..iterations(400) {
        let text = random_text(&mut rng);
        for language in LANGUAGES {
            exercise(language, &text, &mut rng);
        }
    }
}

/// Real-looking samples with random edits: the states typing passes through.
#[test]
fn mutated_samples_never_panic() {
    let samples = [
        include_str!("../src/rust.rs"),
        "[package]\nname = \"x\"\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\ns = \"\"\"\nm\n\"\"\"\n",
        "# T\n\nSome *em* and `code` [l](u).\n\n```rust\nfn x() {}\n```\n- a\n> q\n<!-- c -->\n",
        "{\"a\": [1, 2.5, true, null], \"b\": {\"c\": \"d\"}} // c\n",
        "a: 1\nb:\n  - c: 'x'\n    d: \"y\"\ne: |\n  text\nf: &x {g: [1, 2]}\n",
        "#!/bin/sh\nif [ -f x ]; then\n  echo \"$HOME ${x}\" 'y'\nfi\ncat <<EOF\nbody\nEOF\n",
        C_SAMPLE,
        PYTHON_SAMPLE,
        ZIG_SAMPLE,
        TS_SAMPLE,
        SQL_SAMPLE,
    ];
    let mut rng = Rng(42);
    for _ in 0..iterations(400) / 11 {
        for (k, sample) in samples.iter().enumerate() {
            let mut chars: Vec<char> = sample.chars().collect();
            if k == 0 {
                // The big Rust sample: work on a random window to keep the test fast.
                let start = rng.below(chars.len());
                chars = chars.into_iter().skip(start).take(2000).collect();
            }
            for _ in 0..rng.below(8) {
                let at = rng.below(chars.len() + 1);
                if rng.below(2) == 0 && at < chars.len() {
                    chars.remove(at);
                } else {
                    chars.insert(at, rng.pick(SOUP));
                }
            }
            let text: String = chars.into_iter().collect();
            for language in LANGUAGES {
                exercise(language, &text, &mut rng);
            }
        }
    }
}

/// Inputs that would be quadratic for a naive scanner must stay fast.
#[test]
fn adversarial_inputs_stay_linear() {
    const N: usize = 50_000;
    let cases: Vec<String> = vec![
        "[".repeat(N),
        "]".repeat(N),
        "#[".repeat(N / 2),
        "use ".repeat(N / 4),
        "impl<".repeat(N / 5),
        "* a ".repeat(N / 4),
        "*a ".repeat(N / 3),
        "_a ".repeat(N / 3),
        "`a ".repeat(N / 3),
        "[a](".repeat(N / 4),
        "<a ".repeat(N / 3),
        "<!-- ".repeat(N / 5),
        "\"".repeat(N),
        "/*".repeat(N / 2),
        "${".repeat(N / 2),
        "a = [\n".repeat(N / 6),
        "- ".repeat(N / 2),
        "\n".repeat(N),
        "(".repeat(N / 2) + &"]".repeat(N / 2),
        // Unclosed constructs of the newer lexers.
        "`${".repeat(N / 3),
        "R\"".repeat(N / 2),
        "R\"x(".repeat(N / 4),
        "\"\"\"".repeat(N / 3),
        "'''x".repeat(N / 4),
        "f'{".repeat(N / 3),
        "f\"\"\"{x:{".repeat(N / 8),
        "$$".repeat(N / 2),
        "$a$".repeat(N / 3),
        "/* /* ".repeat(N / 6),
        "(/[".repeat(N / 3),
        "x = /".repeat(N / 5),
        "(<a ".repeat(N / 4),
        "(<a>".repeat(N / 4) + "</a>",
        "#if 0\n".repeat(N / 6),
        "#define A {\n".repeat(N / 12),
        "struct S { ".repeat(N / 10),
        "class A { ".repeat(N / 10),
        "template<".repeat(N / 9),
        "operator".repeat(N / 8),
        "def f():\n".repeat(N / 9),
        "@d\n".repeat(N / 3),
        "begin case ".repeat(N / 11),
        "CREATE TABLE ".repeat(N / 13),
        "const x = struct {".repeat(N / 18),
        "\\\\x\n".repeat(N / 4),
        (0..N / 40).map(|k| format!("{}if x:\n", " ".repeat(k % 200))).collect(),
    ];
    for text in &cases {
        for language in LANGUAGES {
            let start = Instant::now();
            let h = highlight(language, text);
            let _ = folding_ranges(language, text);
            let _ = outline(language, text);
            let _ = matching_bracket(language, text, 0);
            let elapsed = start.elapsed();
            assert_eq!(h.len_lines(), text.matches('\n').count() + 1);
            assert!(
                elapsed < Duration::from_secs(5),
                "{language:?} took {elapsed:?} on {:?}…",
                text.chars().take(12).collect::<String>()
            );
        }
    }
}

/// A realistic file of at least `lines` lines, built by repeating `template`.
fn generated(template: &str, lines: usize) -> String {
    let mut out = String::with_capacity(lines * 40);
    let per = template.lines().count().max(1);
    for _ in 0..lines.div_ceil(per) {
        out.push_str(template);
    }
    out
}

/// A realistic 10k-line Rust file.
fn generated_rust(lines: usize) -> String {
    const TEMPLATE: &str = r##"/// Documentation for the item.
#[derive(Debug, Clone, PartialEq)]
pub struct Point<T> {
    pub x: T, // trailing comment
    pub y: T,
}

impl<T: Copy + std::ops::Add<Output = T>> Point<T> {
    pub const ORIGIN_NAME: &'static str = "origin";

    pub fn new(x: T, y: T) -> Self {
        Self { x, y }
    }

    pub fn sum(&self) -> T {
        let raw = r#"raw "string" with ünïcödé"#;
        println!("{} {:?}", raw, self.x);
        /* block
           comment */
        self.x + self.y
    }
}

fn helper(values: &[u32]) -> Option<u32> {
    values.iter().copied().filter(|v| *v > 0x10 && *v != 0b1010).max()
}
"##;
    generated(TEMPLATE, lines)
}

#[test]
#[ignore = "performance check: run with `cargo test --release -- --ignored`"]
fn highlight_10k_lines_is_fast() {
    let text = generated_rust(10_000);
    let lines = text.lines().count();
    assert!(lines >= 10_000);
    let best = |f: &dyn Fn()| {
        (0..5)
            .map(|_| {
                let start = Instant::now();
                f();
                start.elapsed()
            })
            .min()
            .unwrap_or_default()
    };
    let highlight_time = best(&|| {
        let h = highlight(Language::Rust, &text);
        assert_eq!(h.len_lines(), text.matches('\n').count() + 1);
    });
    let fold_time = best(&|| {
        let _ = folding_ranges(Language::Rust, &text);
    });
    let outline_time = best(&|| {
        let _ = outline(Language::Rust, &text);
    });
    let bracket_time = best(&|| {
        let _ = matching_bracket(Language::Rust, &text, text.len() / 2);
    });
    println!(
        "{lines} lines: highlight {highlight_time:?}, folding {fold_time:?}, outline {outline_time:?}, \
         matching_bracket {bracket_time:?}"
    );
    assert!(highlight_time < Duration::from_millis(50), "highlight took {highlight_time:?}");
}

/// A 10k-line file of each language with a lexer of its own highlights in under 10 ms.
#[test]
#[ignore = "performance check: run with `cargo test --release -- --ignored`"]
fn highlight_10k_lines_every_language() {
    let cases = [
        (Language::C, C_SAMPLE),
        (Language::Cpp, CPP_SAMPLE),
        (Language::Python, PYTHON_SAMPLE),
        (Language::Zig, ZIG_SAMPLE),
        (Language::TypeScript, TS_SAMPLE),
        (Language::JavaScript, TS_SAMPLE),
        (Language::Sql, SQL_SAMPLE),
        (Language::Shell, "if [ -f \"$x\" ]; then\n  echo \"hi $USER\" | grep -c x # count\nfi\n"),
        (Language::Yaml, "key: value\nlist:\n  - 'a'\n  - b: [1, 2]\n"),
    ];
    let best = |f: &dyn Fn()| {
        (0..10)
            .map(|_| {
                let start = Instant::now();
                f();
                start.elapsed()
            })
            .min()
            .unwrap_or_default()
    };
    for (language, template) in cases {
        let text = generated(template, 10_000);
        let lines = text.lines().count();
        let highlight_time = best(&|| {
            let h = highlight(language, &text);
            assert_eq!(h.len_lines(), text.matches('\n').count() + 1);
        });
        let fold_time = best(&|| {
            let _ = folding_ranges(language, &text);
        });
        let outline_time = best(&|| {
            let _ = outline(language, &text);
        });
        let bracket_time = best(&|| {
            let _ = matching_bracket(language, &text, text.len() / 2);
        });
        println!(
            "{language:?} {lines} lines: highlight {highlight_time:?}, folding {fold_time:?}, outline {outline_time:?}, \
             matching_bracket {bracket_time:?}"
        );
        assert!(highlight_time < Duration::from_millis(10), "{language:?} highlight took {highlight_time:?}");
    }
}

/// `\r\n` input highlights like `\n` input, apart from spans that also cover the `\r`.
#[test]
fn crlf_highlights_like_lf() {
    let samples = [
        (Language::Rust, "// c\nfn main() {\n    let s = \"x\"; /* a\n b */\n}\n"),
        (Language::Toml, "[t]\nk = \"v\" # c\ns = '''\nm\n'''\n"),
        (Language::Markdown, "# Title\n\nSome *em*\n```\ncode\n```\n> q\n"),
        (Language::Json, "{\n  \"k\": [1, true]\n}\n"),
        (Language::Yaml, "k: v\nl:\n  - 1\nb: |\n  text\n"),
        (Language::Shell, "if x; then\n  echo \"$a\" # c\nfi\ncat <<EOF\nbody\nEOF\n"),
        (Language::C, C_SAMPLE),
        (Language::Cpp, C_SAMPLE),
        (Language::Python, PYTHON_SAMPLE),
        (Language::Zig, ZIG_SAMPLE),
        (Language::TypeScript, TS_SAMPLE),
        (Language::JavaScript, TS_SAMPLE),
        (Language::Sql, SQL_SAMPLE),
    ];
    for (language, lf) in samples {
        let crlf = lf.replace('\n', "\r\n");
        let (a, b) = (highlight(language, lf), highlight(language, &crlf));
        assert_eq!(a.len_lines(), b.len_lines());
        for (i, len) in line_lengths(lf).into_iter().enumerate() {
            let clamped: Vec<_> =
                b.line(i).iter().filter(|s| s.start < len).map(|s| (s.start, s.end.min(len), s.kind)).collect();
            let expected: Vec<_> = a.line(i).iter().map(|s| (s.start, s.end, s.kind)).collect();
            assert_eq!(clamped, expected, "{language:?} line {i}");
        }
        assert_eq!(folding_ranges(language, lf), folding_ranges(language, &crlf), "{language:?}");
        assert_eq!(outline(language, lf), outline(language, &crlf), "{language:?}");
    }
}

const C_SAMPLE: &str = r#"#include <stdio.h>
#include "util.h"
#define MAX_LEN 64 /* bytes */

/** A node. */
typedef struct node {
    int value;
    struct node *next; // link
} node_t;

#if 0
dead code, don't care
#endif

static size_t count(const node_t *n) {
    size_t total = 0;
    while (n != NULL) {
        total += n->value > 0x1F ? 1u : 0;
        n = n->next;
    }
    printf("%zu items in \"list\"\n", total);
    return total;
}
"#;

const CPP_SAMPLE: &str = r#"#include <vector>
namespace app {
/// Widget docs.
template <typename T>
class Widget : public Base<T> {
public:
    explicit Widget(int x) : x_(x) {}
    [[nodiscard]] int size() const noexcept { return static_cast<int>(items_.size()); }
    bool operator==(const Widget& o) const { return x_ == o.x_; }
private:
    std::vector<T> items_;
    int x_ = 1'000;
};
auto raw = R"sql(SELECT "x" FROM t)sql";
void run() {
    auto f = [](auto a) { return a * 2.5e3; };
    if (f(1) > 0) { std::puts(u8"ok"); }
}
}
"#;

const PYTHON_SAMPLE: &str = r#"import os
from typing import Optional

MAX_RETRIES = 3

@dataclass
class Client(Base):
    """A client."""

    def __init__(self, url: str, retries: int = MAX_RETRIES) -> None:
        self.url = url  # remember
        self.retries = retries

    async def fetch(self, path: str) -> Optional[bytes]:
        msg = f"GET {self.url}/{path!r:>10} {{literal}}"
        if path.startswith('/') and len(path) > 0x10:
            return await self._get(rb'\d+' + b"x")
        return None
"#;

const ZIG_SAMPLE: &str = r#"const std = @import("std");
const mem = std.mem;

/// A point.
pub const Point = struct {
    x: i32 = 0,
    y: i32,

    pub fn len(self: Point) f32 {
        const s =
            \\multi "line"
            \\string
        ;
        _ = s;
        return @floatFromInt(self.x * self.x + self.y * 0x10);
    }
};

test "point length" {
    try std.testing.expect((Point{ .y = 1 }).len() >= 0.5);
}
"#;

const TS_SAMPLE: &str = r#"import { api } from './api';
const MAX = 10;

/** A service. */
export class Service<T> {
  private cache = new Map<string, T>();
  async load(id: string): Promise<T | undefined> {
    const re = /^[a-z]+\/\d+$/i;
    if (!re.test(id)) return undefined;
    const value = await api.get(`items/${id}?limit=${MAX / 2}`);
    return value ?? this.cache.get(id);
  }
}

export const view = (name: string) => (
  <div className="card" onClick={() => alert(name)}>
    Hello, {name}!
  </div>
);
"#;

const SQL_SAMPLE: &str = r#"-- Users
CREATE TABLE IF NOT EXISTS users (
    id BIGSERIAL PRIMARY KEY,
    email VARCHAR(255) NOT NULL UNIQUE,
    created_at TIMESTAMPTZ DEFAULT now()
);

/* Active users with their order counts */
SELECT u.id, u.email, COUNT(o.id) AS orders
FROM users u
LEFT JOIN orders o ON o.user_id = u.id AND o.status <> 'cancelled'
WHERE u.created_at > $1 AND u.email LIKE 'a%''s'
GROUP BY u.id, u.email
ORDER BY orders DESC
LIMIT 10;

CREATE FUNCTION touch() RETURNS trigger AS $$
BEGIN
  NEW.updated_at := now();
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;
"#;
