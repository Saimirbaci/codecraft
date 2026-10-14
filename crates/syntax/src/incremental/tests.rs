use super::*;
use crate::highlight;

const ALL: [Language; 17] = [
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
    Language::Dockerfile,
    Language::Http,
    Language::PlainText,
    Language::DjangoTemplate,
];

const RUST: &str = r####"#![allow(dead_code)]
//! Crate docs.
use std::collections::{BTreeMap, HashMap};
use crate::foo::Bar;

/* block /* nested */ comment
   spanning lines */
#[derive(
    Clone,
    Debug,
)]
pub struct Point<T> { pub x: T, y: f64 }

/// Docs for `go`.
#[cfg(test)]
pub(crate) async fn go<'a>(p: &'a mut Point<i32>, s: &str) -> Result<(), Error> {
    let raw = r#"raw "string" with # signs"#;
    let other = br##"bytes "# still"##;
    let esc = "tab\t quote\" \u{1F600} end";
    let c = 'x'; let b = b'\n'; let lt: &'static str = "s";
    let v = items.iter().map(|x| x.len()).collect::<Vec<_>>();
    let w = parse::<HashMap<String, Vec<u8>>>(input)?;
    println!("{} {}", v.len(), MAX_SIZE);
    macro_rules! square { ($x:expr) => { $x * $x }; }
    if let Some(q) = p.x.checked_add(1) { self.field = q as u64 + 0x1F_u32; }
    match s { "a" | "b" => {}, _ => unreachable!() }
    union_thing.raw = &raw const x;
    Ok(())
}

impl<T: Clone> Trait for Point<T> where T: Default {
    const MAX_SIZE: usize = 1_000;
    fn method(&self) -> Self { Self { x: T::default(), y: 1.5e3 } }
}

#[test]
fn unbalanced() { let a = (1, [2, {3; } ]
"####;

const TOML: &str = "[package]\nname = \"x\" # comment\nversion = '1.0'\n[dependencies]\nserde = { version = \"1\", features = [\n  \"derive\",\n] }\nmulti = \"\"\"\nline \"one\"\nline two\"\"\"\nlit = '''\nraw\n'''\n[[bin]]\npath = \"src/main.rs\"\ndate = 1979-05-27T07:32:00Z\n";

const MARKDOWN: &str = "# Title\n\nSome *emphasis* and **bold** and `code`.\n\nSetext\n======\n\n```rust\nfn main() {}\n```\n\n- item [link](http://x)\n- item [ref][r]\n\n[r]: http://example.com\n\n<!-- html\ncomment -->\n> quote\n\n    indented code\n";

const JSON: &str = "{\n  \"name\": \"x\",\n  \"list\": [1, 2.5e3, -3, true, false, null],\n  // comment\n  \"nested\": { \"a\": \"esc \\\" quote\", \"b\": [] }\n}\n";

const YAML: &str = "# comment\nname: x\nlist:\n  - a\n  - 'quoted'\n  - \"dq # not comment\"\nblock: |\n  literal\n  text\nfolded: >\n  folded\nanchor: &a 1\nref: *a\n---\nkey: 3.5\n";

const SHELL: &str = "#!/bin/sh\n# comment\nname=\"world $HOME ${x:-y}\"\nif [ -f x ]; then\n  echo 'single' \"$(date)\"\nfi\ncat <<EOF\nheredoc $x\nEOF\nfor i in 1 2; do echo $i; done\nf() { return 0; }\n";

const C: &str = "#include <stdio.h>\n#define MAX 10\n/* block\n comment */\n#if DEBUG\nint dbg;\n#endif\nstatic int count = 0x1F;\nstruct point { int x; char *s; };\nint main(int argc, char **argv) {\n  printf(\"%d\\n\", MAX); // line\n  char c = 'a';\n  return 0;\n}\n";

const CPP: &str = "#include <vector>\nnamespace ns {\ntemplate <typename T>\nclass Box : public Base {\npublic:\n  auto get() const -> T { return std::move(v); }\n  std::vector<int> v{1, 2};\n  R\"(raw\nstring)\";\n};\n}\n";

const PYTHON: &str = "import os\nfrom x import (a,\n  b)\n\nclass Foo(Base):\n    \"\"\"Doc\n    string.\"\"\"\n    def go(self, x: int = 3) -> str:\n        s = f\"{x!r} and {self.y}\"\n        r = rb'raw\\n'\n        return 'q' # comment\n\n@decorator\ndef top(*args, **kw):\n    pass\nCONST = 1_000\n";

const ZIG: &str = "const std = @import(\"std\");\n// comment\npub fn main() !void {\n    const s =\n        \\\\multi\n        \\\\line\n    ;\n    var x: u32 = 0x10;\n    std.debug.print(\"{}\\n\", .{x});\n}\ntest \"name\" { try expect(true); }\n";

const TYPESCRIPT: &str = "import { a } from './a';\ninterface Foo { x: number; }\nconst re = /ab+c/g;\nconst t = `tmpl ${a + `inner ${b}`} end`;\n/* block\n comment */\nexport function go<T>(x: T): T {\n  const y = x / 2 / 3;\n  return <div className=\"c\">{y}</div>;\n}\nclass Bar extends Foo { method() {} }\nconst f = () => 1;\n";

const JAVASCRIPT: &str = "// comment\nconst re = /[/]x/i, d = a / b;\nlet s = 'q' + \"dq\" + `t ${x}`;\nfunction go(a, b = 2) {\n  return a?.b ?? null;\n}\nconst f = async (x) => { await x; };\n";

const SQL: &str = "-- comment\nCREATE TABLE users (\n  id INT PRIMARY KEY,\n  name TEXT NOT NULL\n);\n/* block\n comment */\nSELECT u.name, 'str''q' FROM users u WHERE id = 1;\nCREATE FUNCTION f() RETURNS int AS $$\nBEGIN\n  RETURN 1;\nEND;\n$$ LANGUAGE plpgsql;\n";

const DOCKERFILE: &str = "# syntax=docker/dockerfile:1\nFROM rust:1 AS build\nWORKDIR /app\nCOPY . .\nRUN cargo build --release && \\\n    strip target/release/app\nENV RUST_LOG=info\nCMD [\"/app/target/release/app\"]\n";

const HTTP: &str = "@host = localhost\n### Users\n# @name list\nGET http://{{host}}/users HTTP/1.1\nAccept: application/json\n\n###\nPOST /x\nContent-Type: application/json\n\n{\"a\": \"{{$uuid}}\"}\n\n> {%\n  client.log(1);\n%}\n";

const DJANGO: &str = "{% extends \"base.html\" %}\n{% block content %}\n<div class=\"a {{ cls }}\">\n  {# note #}\n  {% for x in xs|sort %}<p>{{ x.name|title }}</p>{% endfor %}\n</div>\n<!-- c -->\n{% endblock %}\n";

const PLAIN: &str = "just some text\nwith lines\n\nand more\n";

fn sample(language: Language) -> &'static str {
    match language {
        Language::Rust => RUST,
        Language::Toml => TOML,
        Language::Markdown => MARKDOWN,
        Language::Json => JSON,
        Language::Yaml => YAML,
        Language::Shell => SHELL,
        Language::C => C,
        Language::Cpp => CPP,
        Language::Python => PYTHON,
        Language::Zig => ZIG,
        Language::TypeScript => TYPESCRIPT,
        Language::JavaScript => JAVASCRIPT,
        Language::Sql => SQL,
        Language::Dockerfile => DOCKERFILE,
        Language::Http => HTTP,
        Language::PlainText => PLAIN,
        Language::DjangoTemplate => DJANGO,
    }
}

/// Token-ish snippets for random edits.
#[rustfmt::skip]
const SNIPPETS: &[&str] = &[
    "\"", "'", "/*", "*/", "//", "#[", "#![", "]", "[", "(", ")", "{", "}", "\n", "r#\"", "\"#", "::<", ">", "<",
    "fn ", "let ", "x", "foo", "Bar", "MAX", "!", "(", "é", "😀", " ", "    ", "\t", "1.5", "0x1F", "b'", "r", "#",
    "\\", "`", "${", "$$", "\"\"\"", "'''", "-- ", "<!--", "-->", "```", "#define X\n", "=\n", ": ", ",", ";", ".",
    "->", "=>", "&'a ", "macro_rules! m ", "println!(", "\\\\", "@import", "f\"", "<div>", "</div>", "$x", "EOF\n",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

fn byte_of(text: &str, ch: usize) -> usize {
    text.char_indices().nth(ch).map_or(text.len(), |(b, _)| b)
}

/// Replace chars `start..end` with `insert`; returns the new end in chars.
fn splice(text: &mut String, start: usize, end: usize, insert: &str) -> usize {
    let (bs, be) = (byte_of(text, start), byte_of(text, end));
    text.replace_range(bs..be, insert);
    start + insert.chars().count()
}

fn random_edit(rng: &mut Rng, text: &str) -> (usize, usize, String) {
    let len = text.chars().count();
    let start = rng.below(len + 1);
    let del = match rng.below(10) {
        0..=3 => 0,
        4..=8 => rng.below(6),
        _ => rng.below(60),
    };
    let end = (start + del).min(len);
    let mut insert = String::new();
    if rng.below(5) != 0 {
        for _ in 0..1 + rng.below(3) {
            insert.push_str(SNIPPETS[rng.below(SNIPPETS.len())]);
        }
    }
    (start, end, insert)
}

/// Lines outside `range` are the old ones (prefix as-is, suffix shifted).
fn assert_range_covers(old: &Highlights, new: &Highlights, range: &Range<usize>, ctx: &str) {
    let (a, b) = (&old.lines, &new.lines);
    assert!(range.start <= range.end && range.end <= b.len(), "{ctx}: bad range {range:?} for {} lines", b.len());
    for l in 0..range.start {
        assert_eq!(a.get(l), b.get(l), "{ctx}: prefix line {l} changed outside {range:?}");
    }
    let tail = b.len() - range.end;
    assert!(tail <= a.len(), "{ctx}: tail longer than old");
    for k in 0..tail {
        assert_eq!(
            a[a.len() - 1 - k],
            b[b.len() - 1 - k],
            "{ctx}: suffix line {} changed outside {range:?}",
            b.len() - 1 - k
        );
    }
}

fn check_rust_state(h: &IncrementalHighlighter, text: &str, ctx: &str) {
    let st = h.rust.as_ref().expect("rust state");
    let toks = rust::lex(text);
    assert_eq!(st.toks, toks, "{ctx}: tokens");
    assert_eq!(st.kinds, rust::classify(text, &toks), "{ctx}: kinds");
    let mut fresh = IncrementalHighlighter::new(Language::Rust, text);
    assert_eq!(st.marks, fresh.rust.take().unwrap().marks, "{ctx}: loop marks");
}

fn run_random(language: Language, seed: u64, steps: usize, initial: &str) {
    let mut rng = Rng(seed);
    let mut text = initial.to_string();
    let mut h = IncrementalHighlighter::new(language, &text);
    let mut fallbacks = 0usize;
    assert_eq!(h.highlights(), &highlight(language, &text));
    for step in 0..steps {
        let (start, end, insert) = random_edit(&mut rng, &text);
        let new_end = splice(&mut text, start, end, &insert);
        let old = h.highlights().clone();
        let range = if language == Language::Rust {
            // Call the incremental path directly so a silent fallback would be noticed.
            match h.update_rust(&text, start, end, new_end) {
                Some(range) => range,
                None => {
                    fallbacks += 1;
                    h.reset(language, &text);
                    0..h.highlights().len_lines()
                }
            }
        } else {
            h.update(&text, start, end, new_end)
        };
        let ctx = format!("{language:?} seed {seed} step {step} edit {start}..{end} -> {insert:?}");
        assert_eq!(h.highlights(), &highlight(language, &text), "{ctx}\ntext:\n{text}");
        assert_range_covers(&old, h.highlights(), &range, &ctx);
        if language == Language::Rust {
            check_rust_state(&h, &text, &ctx);
        }
    }
    assert_eq!(h.folding_ranges(&text), crate::folding_ranges(language, &text));
    // Only texts starting with `#!` take the full path.
    assert!(fallbacks <= steps / 20, "{language:?} seed {seed}: {fallbacks} fallbacks");
}

#[test]
fn random_edits_match_a_full_lex_in_every_language() {
    for (i, language) in ALL.into_iter().enumerate() {
        run_random(language, 0x9E37_79B9_7F4A_7C15 ^ i as u64, 300, sample(language));
    }
}

#[test]
fn random_edits_on_rust_many_seeds() {
    // A longer text so the classification window does not reach the ends.
    let long = RUST.repeat(4);
    for seed in 1..=12u64 {
        run_random(Language::Rust, seed.wrapping_mul(0x1234_5678_9ABC_DEF1), 250, &long);
    }
}

#[test]
fn typing_a_file_char_by_char_matches() {
    let mut text = String::new();
    let mut h = IncrementalHighlighter::new(Language::Rust, &text);
    for (i, c) in RUST.chars().enumerate() {
        let mut buf = [0u8; 4];
        splice(&mut text, i, i, c.encode_utf8(&mut buf));
        h.update(&text, i, i, i + 1);
        assert_eq!(h.highlights(), &highlight(Language::Rust, &text), "after {i} chars");
    }
    check_rust_state(&h, &text, "typed");
    // And deleting it back to front.
    let mut len = text.chars().count();
    while len > 0 {
        splice(&mut text, len - 1, len, "");
        h.update(&text, len - 1, len, len - 1);
        len -= 1;
        assert_eq!(h.highlights(), &highlight(Language::Rust, &text), "at {len} chars");
    }
}

#[test]
fn attributes_and_turbofish_far_from_the_edit() {
    // An attribute left open at the top: closing it at the bottom changes the first lines.
    let body = "fn f() { let x = 1; }\n".repeat(60);
    let mut text = format!("#[cfg(test\n{body}");
    let mut h = IncrementalHighlighter::new(Language::Rust, &text);
    let at = text.chars().count();
    let new_end = splice(&mut text, at, at, ")]");
    let range = h.update(&text, at, at, new_end);
    assert_eq!(h.highlights(), &highlight(Language::Rust, &text));
    assert_eq!(range.start, 0);
    // A turbofish whose `(` arrives 60 tokens later.
    let mut text = format!("f::<{}>\n", "A, ".repeat(25));
    let mut h = IncrementalHighlighter::new(Language::Rust, &text);
    let at = text.chars().count();
    let new_end = splice(&mut text, at, at, "(1)");
    h.update(&text, at, at, new_end);
    assert_eq!(h.highlights(), &highlight(Language::Rust, &text));
    check_rust_state(&h, &text, "turbofish");
}

#[test]
fn shebang_and_empty_texts() {
    let mut text = String::from("fn main() {}\n");
    let mut h = IncrementalHighlighter::new(Language::Rust, &text);
    for (start, end, ins) in [(0, 0, "#!/usr/bin/env run\n"), (0, 2, ""), (0, 0, "#![attr]\n"), (0, 1, "")] {
        let new_end = splice(&mut text, start, end, ins);
        h.update(&text, start, end, new_end);
        assert_eq!(h.highlights(), &highlight(Language::Rust, &text), "{text:?}");
    }
    let len = text.chars().count();
    splice(&mut text, 0, len, "");
    h.update(&text, 0, len, 0);
    assert_eq!(h.highlights(), &highlight(Language::Rust, ""));
    splice(&mut text, 0, 0, "x");
    h.update(&text, 0, 0, 1);
    assert_eq!(h.highlights(), &highlight(Language::Rust, "x"));
}

#[test]
fn bad_inputs_fall_back_without_panicking() {
    for language in ALL {
        let text = sample(language);
        let n = text.chars().count();
        for (start, old_end, new_end) in
            [(5, 2, 9), (0, n + 10, 3), (n + 5, n + 6, n + 7), (usize::MAX, usize::MAX, usize::MAX), (3, 3, usize::MAX)]
        {
            let mut h = IncrementalHighlighter::new(language, "fn x() {}\nlet y;\n");
            let range = h.update(text, start, old_end, new_end);
            assert!(range.end <= h.highlights().len_lines());
            // Out-of-range edits are detected and re-lexed in full.
            if old_end > "fn x() {}\nlet y;\n".len() || start > old_end {
                assert_eq!(h.highlights(), &highlight(language, text), "{language:?} {start} {old_end} {new_end}");
            }
        }
        // A wrongly described (but in-range) edit never panics.
        let mut h = IncrementalHighlighter::new(language, text);
        let _ = h.update("totally different 😀 text\n", 1, 2, 3);
        let _ = h.update("", 0, 0, 0);
        h.reset(language, text);
        assert_eq!(h.highlights(), &highlight(language, text));
    }
}

#[test]
fn reset_switches_language() {
    let mut h = IncrementalHighlighter::new(Language::Rust, RUST);
    h.reset(Language::Python, PYTHON);
    assert_eq!(h.language(), Language::Python);
    assert_eq!(h.highlights(), &highlight(Language::Python, PYTHON));
    h.reset(Language::Rust, RUST);
    assert_eq!(h.highlights(), &highlight(Language::Rust, RUST));
}

#[test]
fn rust_update_only_rebuilds_nearby_lines() {
    let text = RUST.repeat(40);
    let mut h = IncrementalHighlighter::new(Language::Rust, &text);
    let mut text2 = text.clone();
    let at_ch = text.chars().count() / 2;
    splice(&mut text2, at_ch, at_ch, "z");
    let range = h.update(&text2, at_ch, at_ch, at_ch + 1);
    assert_eq!(h.highlights(), &highlight(Language::Rust, &text2));
    assert!(range.len() < 40, "rebuilt {range:?}");
}

/// Timing: `cargo test -p codecraft-syntax --release -- --ignored --nocapture incremental_timing`.
#[test]
#[ignore]
fn incremental_timing() {
    use std::time::Instant;
    let text = RUST.repeat(10_000 / RUST.lines().count() + 1);
    let lines = text.lines().count();
    let t = Instant::now();
    let full = highlight(Language::Rust, &text);
    let full_time = t.elapsed();
    let mut h = IncrementalHighlighter::new(Language::Rust, &text);
    let mut text2 = text.clone();
    let mut rng = Rng(7);
    let n = 200;
    let mut spent = std::time::Duration::ZERO;
    for _ in 0..n {
        let len = text2.chars().count();
        let at = rng.below(len);
        let new_end = splice(&mut text2, at, at, "x");
        let t = Instant::now();
        h.update(&text2, at, at, new_end);
        spent += t.elapsed();
    }
    let per_update = spent / n;
    eprintln!("{lines} lines: full highlight {full_time:?}, incremental update {per_update:?}");
    assert_eq!(full.len_lines(), lines + 1);
    assert_eq!(h.highlights(), &highlight(Language::Rust, &text2));
}

/// Apply one edit through the incremental Rust path (no silent fallback) and compare.
fn rust_edit(text: &mut String, h: &mut IncrementalHighlighter, start: usize, end: usize, insert: &str) {
    let new_end = splice(text, start, end, insert);
    let range = h.update_rust(text, start, end, new_end);
    assert!(range.is_some(), "fell back on {text:?}");
    assert_eq!(h.highlights(), &highlight(Language::Rust, text), "{text:?}");
    check_rust_state(h, text, "edit");
}

#[test]
fn lookbehind_two_tokens_before_the_tail() {
    // `y` is a field after `q .` but not after `. .` (a range): the change is two tokens back.
    let pad = "let pad = 1;\n".repeat(3);
    let mut text = format!("{pad}q .y + z");
    let mut h = IncrementalHighlighter::new(Language::Rust, &text);
    let at = pad.chars().count();
    rust_edit(&mut text, &mut h, at, at + 1, ".");
    rust_edit(&mut text, &mut h, at, at + 1, "q");
}

#[test]
fn turbofish_lookahead_at_every_length() {
    for n in 50..=70 {
        let pad = "let pad = 1;\n".repeat(2);
        let mut text = format!("{pad}f::<{}>\n  x", "Abcdefghijklmnop,".repeat(n / 2));
        let mut h = IncrementalHighlighter::new(Language::Rust, &text);
        let at = text.chars().count() - 1;
        rust_edit(&mut text, &mut h, at, at + 1, "(1)");
        rust_edit(&mut text, &mut h, at, at + 3, "");
    }
}
