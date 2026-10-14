//! Tools › Markdown › Export Markdown File To…: the active (or given) Markdown file as a
//! standalone HTML page.
//!
//! The converter covers the CommonMark/GFM subset documents use: ATX headings, paragraphs, fenced
//! and indented code, block quotes, nested bullet and numbered lists, task lists, tables,
//! horizontal rules, and inline code, emphasis, strong, strikethrough, links, images, autolinks
//! and hard line breaks. Text is HTML-escaped; `javascript:` and other script URLs are dropped.
//! PDF export is not offered (print the HTML from a browser instead).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use codecraft_syntax::Language;

use crate::commands::{CmdResult, CommandSpec, Context, spec};
use crate::params::opt_path;
use crate::{CommandError, Effect, Session};

/// Largest Markdown input converted (chars).
const MAX_INPUT: usize = 8 << 20;

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn safe_url(url: &str) -> String {
    let u = url.trim();
    let lower: String = u.chars().filter(|c| !c.is_whitespace() && !c.is_control()).collect::<String>().to_ascii_lowercase();
    if lower.starts_with("javascript:") || lower.starts_with("vbscript:") || lower.starts_with("data:text/html") {
        return "#".into();
    }
    escape(u)
}

/// Inline Markdown → HTML.
pub fn inline(text: &str) -> String {
    inline_at(text, 0)
}

/// Nesting deeper than this is written as plain text (hostile input cannot overflow the stack).
const MAX_NESTING: usize = 32;

fn inline_at(text: &str, depth: usize) -> String {
    if depth > MAX_NESTING {
        return escape(text);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    let rest = |i: usize| -> String { chars.get(i..).map(|c| c.iter().collect()).unwrap_or_default() };
    let find = |from: usize, pat: &str| -> Option<usize> {
        let p: Vec<char> = pat.chars().collect();
        (from..chars.len()).find(|&k| chars.get(k..k + p.len()) == Some(p.as_slice()))
    };
    let slice = |a: usize, b: usize| -> String { chars.get(a..b).map(|c| c.iter().collect()).unwrap_or_default() };
    while let Some(&c) = chars.get(i) {
        match c {
            '\\' if chars.get(i + 1).is_some_and(|n| n.is_ascii_punctuation()) => {
                out.push_str(&escape(&chars.get(i + 1).map(|n| n.to_string()).unwrap_or_default()));
                i += 2;
            }
            '`' => {
                let ticks = chars.get(i..).map(|r| r.iter().take_while(|x| **x == '`').count()).unwrap_or(1);
                let fence = "`".repeat(ticks);
                match find(i + ticks, &fence) {
                    Some(end) => {
                        out.push_str("<code>");
                        out.push_str(&escape(slice(i + ticks, end).trim()));
                        out.push_str("</code>");
                        i = end + ticks;
                    }
                    None => {
                        out.push_str(&fence);
                        i += ticks;
                    }
                }
            }
            '!' if chars.get(i + 1) == Some(&'[') => match link_at(&chars, i + 1) {
                Some((alt, url, end)) => {
                    out.push_str(&format!("<img src=\"{}\" alt=\"{}\">", safe_url(&url), escape(&alt)));
                    i = end;
                }
                None => {
                    out.push('!');
                    i += 1;
                }
            },
            '[' => match link_at(&chars, i) {
                Some((label, url, end)) => {
                    out.push_str(&format!("<a href=\"{}\">{}</a>", safe_url(&url), inline_at(&label, depth + 1)));
                    i = end;
                }
                None => {
                    out.push('[');
                    i += 1;
                }
            },
            '<' if rest(i + 1).starts_with("http://") || rest(i + 1).starts_with("https://") => match find(i + 1, ">") {
                Some(end) => {
                    let url = slice(i + 1, end);
                    out.push_str(&format!("<a href=\"{}\">{}</a>", safe_url(&url), escape(&url)));
                    i = end + 1;
                }
                None => {
                    out.push_str("&lt;");
                    i += 1;
                }
            },
            '*' | '_' | '~' => {
                let double = chars.get(i + 1) == Some(&c);
                let (tag, len) = match (c, double) {
                    ('~', true) => ("del", 2),
                    ('~', false) => {
                        out.push('~');
                        i += 1;
                        continue;
                    }
                    (_, true) => ("strong", 2),
                    _ => ("em", 1),
                };
                let delim: String = std::iter::repeat_n(c, len).collect();
                let opens = chars.get(i + len).is_some_and(|n| !n.is_whitespace());
                // `snake_case` words are not emphasis.
                let intraword = c == '_' && i > 0 && chars.get(i - 1).is_some_and(|p| p.is_alphanumeric());
                match find(i + len, &delim).filter(|&e| opens && !intraword && e > i + len) {
                    Some(end) => {
                        out.push_str(&format!("<{tag}>{}</{tag}>", inline_at(&slice(i + len, end), depth + 1)));
                        i = end + len;
                    }
                    None => {
                        out.push_str(&delim);
                        i += len;
                    }
                }
            }
            '\n' => {
                if out.ends_with("  ") {
                    let trimmed = out.trim_end_matches(' ').len();
                    out.truncate(trimmed);
                    out.push_str("<br>\n");
                } else {
                    out.push('\n');
                }
                i += 1;
            }
            _ => {
                out.push_str(&escape(&c.to_string()));
                i += 1;
            }
        }
    }
    out
}

/// `[label](url)` at `i` → (label, url, index after it).
fn link_at(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    let mut depth = 0usize;
    let mut k = i;
    let close = loop {
        match chars.get(k)? {
            '[' => depth += 1,
            ']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    break k;
                }
            }
            _ => {}
        }
        k += 1;
    };
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = (close + 2..chars.len()).find(|&j| chars.get(j) == Some(&')'))?;
    let label: String = chars.get(i + 1..close)?.iter().collect();
    let target: String = chars.get(close + 2..end)?.iter().collect();
    // `(url "title")`: the URL is the first word.
    let url = target.split_whitespace().next().unwrap_or("").trim_matches(['<', '>']).to_string();
    Some((label, url, end + 1))
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return None;
    }
    let level = t.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = t.get(level..)?;
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    Some((level, rest.trim().trim_end_matches('#').trim_end()))
}

fn is_rule(line: &str) -> bool {
    let t: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_'))
}

/// A list item: (indent, ordered, content).
fn list_item(line: &str) -> Option<(usize, bool, &str)> {
    let indent = line.len() - line.trim_start().len();
    let t = line.trim_start();
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(m) {
            return Some((indent, false, rest));
        }
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if (1..=9).contains(&digits) {
        let rest = t.get(digits..)?;
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some((indent, true, r));
        }
    }
    None
}

fn table_cells(line: &str) -> Vec<String> {
    let t = line.trim().trim_start_matches('|').trim_end_matches('|');
    t.split('|').map(|c| c.trim().to_string()).collect()
}

fn is_table_separator(line: &str) -> bool {
    let cells = table_cells(line);
    !cells.is_empty() && cells.iter().all(|c| !c.is_empty() && c.chars().all(|x| matches!(x, '-' | ':' | ' ')) && c.contains('-'))
}

/// Markdown → an HTML body fragment.
pub fn to_html(markdown: &str) -> String {
    to_html_at(markdown, 0)
}

fn to_html_at(markdown: &str, depth: usize) -> String {
    if depth > MAX_NESTING {
        return format!("<pre>{}</pre>\n", escape(markdown));
    }
    let lines: Vec<&str> = markdown.lines().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while let Some(&line) = lines.get(i) {
        let trimmed = line.trim_start();
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        // Fenced code.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let fence: String = trimmed.chars().take_while(|c| *c == '`' || *c == '~').collect();
            let lang = trimmed.get(fence.len()..).unwrap_or("").trim();
            let mut code = Vec::new();
            i += 1;
            while let Some(&l) = lines.get(i) {
                i += 1;
                if l.trim_start().starts_with(&fence) {
                    break;
                }
                code.push(l);
            }
            let class = if lang.is_empty() { String::new() } else { format!(" class=\"language-{}\"", escape(lang.split_whitespace().next().unwrap_or(""))) };
            out.push_str(&format!("<pre><code{class}>{}</code></pre>\n", escape(&code.join("\n"))));
            continue;
        }
        if let Some((level, text)) = heading(line) {
            out.push_str(&format!("<h{level}>{}</h{level}>\n", inline(text)));
            i += 1;
            continue;
        }
        if is_rule(line) {
            out.push_str("<hr>\n");
            i += 1;
            continue;
        }
        if trimmed.starts_with('>') {
            let mut quoted = Vec::new();
            while let Some(&l) = lines.get(i) {
                let t = l.trim_start();
                if !t.starts_with('>') {
                    break;
                }
                quoted.push(t.trim_start_matches('>').strip_prefix(' ').unwrap_or(t.trim_start_matches('>')));
                i += 1;
            }
            out.push_str(&format!("<blockquote>\n{}</blockquote>\n", to_html_at(&quoted.join("\n"), depth + 1)));
            continue;
        }
        if line.starts_with("    ") || line.starts_with('\t') {
            let mut code = Vec::new();
            while let Some(&l) = lines.get(i) {
                if !(l.starts_with("    ") || l.starts_with('\t') || l.trim().is_empty()) {
                    break;
                }
                code.push(l.strip_prefix("    ").or_else(|| l.strip_prefix('\t')).unwrap_or(""));
                i += 1;
            }
            while code.last().is_some_and(|l| l.trim().is_empty()) {
                code.pop();
            }
            out.push_str(&format!("<pre><code>{}</code></pre>\n", escape(&code.join("\n"))));
            continue;
        }
        if line.contains('|') && lines.get(i + 1).is_some_and(|n| is_table_separator(n)) {
            let head = table_cells(line);
            out.push_str("<table>\n<thead><tr>");
            for c in &head {
                out.push_str(&format!("<th>{}</th>", inline(c)));
            }
            out.push_str("</tr></thead>\n<tbody>\n");
            i += 2;
            while let Some(&l) = lines.get(i) {
                if l.trim().is_empty() || !l.contains('|') {
                    break;
                }
                out.push_str("<tr>");
                for c in table_cells(l) {
                    out.push_str(&format!("<td>{}</td>", inline(&c)));
                }
                out.push_str("</tr>\n");
                i += 1;
            }
            out.push_str("</tbody>\n</table>\n");
            continue;
        }
        if let Some((_, ordered, _)) = list_item(line) {
            let start = i;
            while let Some(&l) = lines.get(i) {
                if l.trim().is_empty() {
                    // A blank line ends the list unless an item of the same kind follows.
                    if lines.get(i + 1).and_then(|n| list_item(n)).is_some_and(|(_, o, _)| o == ordered) {
                        i += 1;
                        continue;
                    }
                    break;
                }
                if list_item(l).is_none() && !l.starts_with(' ') {
                    break;
                }
                i += 1;
            }
            out.push_str(&list_html(lines.get(start..i).unwrap_or(&[])));
            continue;
        }
        // Paragraph: until a blank line or another block.
        let mut para = Vec::new();
        while let Some(&l) = lines.get(i) {
            if l.trim().is_empty() || heading(l).is_some() || l.trim_start().starts_with("```") || l.trim_start().starts_with('>') || (!para.is_empty() && list_item(l).is_some()) || (is_rule(l) && !para.is_empty() && l.trim().chars().all(|c| c == '*')) {
                break;
            }
            para.push(l);
            i += 1;
        }
        if para.is_empty() {
            i += 1;
            continue;
        }
        out.push_str(&format!("<p>{}</p>\n", inline(&para.join("\n"))));
    }
    out
}

/// A run of list lines (items and their continuation lines) → nested `<ul>`/`<ol>`.
fn list_html(lines: &[&str]) -> String {
    // (indent, ordered, text) per item; continuation lines join the previous item.
    let mut items: Vec<(usize, bool, String)> = Vec::new();
    for l in lines {
        match list_item(l) {
            Some((indent, ordered, text)) => items.push((indent, ordered, text.to_string())),
            None => {
                if let Some(last) = items.last_mut() {
                    last.2.push('\n');
                    last.2.push_str(l.trim());
                }
            }
        }
    }
    let mut out = String::new();
    let mut stack: Vec<(usize, bool)> = Vec::new();
    for (indent, ordered, text) in items {
        while stack.last().is_some_and(|(d, _)| indent < *d) {
            if let Some((_, o)) = stack.pop() {
                out.push_str(if o { "</li></ol>\n" } else { "</li></ul>\n" });
            }
        }
        match stack.last() {
            Some((d, _)) if indent == *d => out.push_str("</li>\n"),
            Some((d, _)) if indent > *d && stack.len() < 16 => {
                stack.push((indent, ordered));
                out.push_str(if ordered { "\n<ol>\n" } else { "\n<ul>\n" });
            }
            Some(_) => out.push_str("</li>\n"),
            None => {
                stack.push((indent, ordered));
                out.push_str(if ordered { "<ol>\n" } else { "<ul>\n" });
            }
        }
        let (check, body) = if let Some(r) = text.strip_prefix("[ ] ") {
            ("<input type=\"checkbox\" disabled> ", r.to_string())
        } else if let Some(r) = text.strip_prefix("[x] ").or_else(|| text.strip_prefix("[X] ")) {
            ("<input type=\"checkbox\" checked disabled> ", r.to_string())
        } else {
            ("", text.clone())
        };
        out.push_str(&format!("<li>{check}{}", inline(&body)));
    }
    while let Some((_, o)) = stack.pop() {
        out.push_str(if o { "</li></ol>\n" } else { "</li></ul>\n" });
    }
    out
}

/// A standalone HTML page for a Markdown document.
pub fn page(markdown: &str, fallback_title: &str) -> String {
    let title = markdown.lines().find_map(heading).map(|(_, t)| t.to_string()).unwrap_or_else(|| fallback_title.to_string());
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{}</title>\n<style>\n{STYLE}</style>\n</head>\n<body>\n<main>\n{}</main>\n</body>\n</html>\n",
        escape(&title),
        to_html(markdown)
    )
}

const STYLE: &str = ":root { color-scheme: light dark; --fg: #1f2328; --bg: #ffffff; --muted: #59636e; --code: #f3f4f6; --line: #d1d9e0; --link: #0969da; }
@media (prefers-color-scheme: dark) { :root { --fg: #dfe1e5; --bg: #1e1f22; --muted: #9da0a8; --code: #2b2d30; --line: #43454a; --link: #589df6; } }
body { margin: 0; background: var(--bg); color: var(--fg); font: 16px/1.6 -apple-system, BlinkMacSystemFont, \"Segoe UI\", sans-serif; }
main { max-width: 860px; margin: 0 auto; padding: 32px 16px 64px; }
a { color: var(--link); }
code { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.9em; background: var(--code); padding: 0.1em 0.3em; border-radius: 4px; }
pre { background: var(--code); padding: 12px 16px; border-radius: 6px; overflow-x: auto; }
pre code { background: none; padding: 0; }
blockquote { margin: 0; padding: 0 1em; color: var(--muted); border-left: 4px solid var(--line); }
table { border-collapse: collapse; } th, td { border: 1px solid var(--line); padding: 6px 12px; }
hr { border: 0; border-top: 1px solid var(--line); }
img { max-width: 100%; }
";

fn export(s: &mut Session, p: &Value) -> CmdResult {
    let source: PathBuf = match opt_path(p, "path") {
        Some(p) => crate::normalize_path(&p),
        None => s.ws.active().and_then(|(_, d)| d.path.clone()).ok_or_else(|| CommandError::Failed("the editor has no file".into()))?,
    };
    let Some(output) = opt_path(p, "output") else {
        let name = source.file_stem().map(|n| format!("{}.html", n.to_string_lossy())).unwrap_or_else(|| "export.html".into());
        s.effects.push(Effect::PickFile {
            title: "Export Markdown File To".into(),
            command: "tools.markdown.exportMarkdownFileTo".into(),
            param: "output".into(),
            save: true,
            extensions: vec!["html".into()],
            default_name: Some(name),
        });
        // The picked path comes back with `output`; remember the source by opening nothing new.
        return Ok(json!({ "picker": true, "source": source }));
    };
    let text = s.current_text(&source).ok_or_else(|| CommandError::Failed(format!("{} cannot be read", source.display())))?;
    if text.chars().count() > MAX_INPUT {
        return Err(CommandError::Failed("the file is too large to export".into()));
    }
    let title = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let html = page(&text, &title);
    let output = if output.extension().is_none() { output.with_extension("html") } else { output };
    if output == source {
        return Err(CommandError::BadParams("the output would overwrite the Markdown file".into()));
    }
    codecraft_fs::write_text_file_atomic(&output, &html).map_err(|e| CommandError::Failed(format!("cannot write {}: {e}", output.display())))?;
    s.status = format!("Exported {} to {}", title, output.display());
    Ok(json!({ "output": output }))
}

fn is_markdown(s: &Session) -> Result<(), String> {
    match s.ws.active() {
        Some((_, d)) if d.language == Language::Markdown || d.path.as_deref().is_some_and(|p| Language::from_path(p) == Language::Markdown) => Ok(()),
        Some(_) => Err("the active file is not Markdown".into()),
        None => Err("no editor is open".into()),
    }
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![spec(
        "tools.markdown.exportMarkdownFileTo",
        "Export Markdown File To…",
        None,
        "{path?: string (default: the active file), output?: string} — write the Markdown file as a standalone HTML page; without `output` asks where",
        Context::Global,
        is_markdown,
        export,
    )]
}

/// Whether a path is a Markdown file.
pub fn markdown_path(p: &Path) -> bool {
    Language::from_path(p) == Language::Markdown
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn blocks() {
        let md = "# Title\n\nSome *em* and **strong** and `code` and ~~gone~~.\nNext line  \nbroken.\n\n- a\n- b\n  - b1\n- [x] done\n\n1. one\n2. two\n\n> quote\n\n```rust\nfn x() -> u8 { 1 < 2 }\n```\n\n| a | b |\n|---|:-:|\n| 1 | 2 |\n\n---\n\n    indented\n";
        let h = to_html(md);
        assert!(h.contains("<h1>Title</h1>"));
        assert!(h.contains("<em>em</em>") && h.contains("<strong>strong</strong>") && h.contains("<code>code</code>") && h.contains("<del>gone</del>"));
        assert!(h.contains("Next line<br>\nbroken."));
        assert!(h.contains("<ul>\n<li>a</li>\n<li>b\n<ul>\n<li>b1</li></ul>\n</li>\n<li><input type=\"checkbox\" checked disabled> done</li></ul>"), "{h}");
        assert!(h.contains("<ol>\n<li>one</li>\n<li>two</li></ol>"));
        assert!(h.contains("<blockquote>\n<p>quote</p>\n</blockquote>"));
        assert!(h.contains("<pre><code class=\"language-rust\">fn x() -&gt; u8 { 1 &lt; 2 }</code></pre>"));
        assert!(h.contains("<th>a</th><th>b</th>") && h.contains("<td>1</td><td>2</td>"));
        assert!(h.contains("<hr>"));
        assert!(h.contains("<pre><code>indented</code></pre>"));
    }

    #[test]
    fn inline_links_are_safe() {
        assert_eq!(inline("[a](https://x.dev \"t\")"), "<a href=\"https://x.dev\">a</a>");
        assert_eq!(inline("[a](javascript:alert(1))"), "<a href=\"#\">a</a>)");
        assert_eq!(inline("![i](p.png)"), "<img src=\"p.png\" alt=\"i\">");
        assert_eq!(inline("<https://x.dev>"), "<a href=\"https://x.dev\">https://x.dev</a>");
        assert_eq!(inline("<script>"), "&lt;script&gt;");
        assert_eq!(inline("snake_case_name"), "snake_case_name");
        assert_eq!(inline("\\*not\\*"), "*not*");
        for t in ["*", "**", "[", "[x](", "`", "``x`", "![", "\\", "~", "_a", "😀*x*"] {
            let _ = inline(t);
            let _ = to_html(t);
        }
        let _ = to_html(&"- x\n".repeat(50));
        let _ = to_html(&format!("{}x", "  ".repeat(100)));
        let nested: String = (0..40).map(|d| format!("{}- {d}\n", "  ".repeat(d))).collect();
        let _ = to_html(&nested);
    }

    #[test]
    fn export_writes_a_page() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("README.md");
        std::fs::write(&src, "# Hello <World>\n\nText.\n").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.execute("file.open", &json!({ "path": src })).unwrap();
        assert_eq!(s.execute("tools.markdown.exportMarkdownFileTo", &json!({})).unwrap()["picker"], true);
        assert!(matches!(s.effects.last(), Some(Effect::PickFile { default_name: Some(n), .. }) if n == "README.html"));
        let out = tmp.path().join("out");
        let v = s.execute("tools.markdown.exportMarkdownFileTo", &json!({ "output": out })).unwrap();
        let written = PathBuf::from(v["output"].as_str().unwrap());
        assert_eq!(written, tmp.path().join("out.html"));
        let html = std::fs::read_to_string(written).unwrap();
        assert!(html.contains("<title>Hello &lt;World&gt;</title>"));
        assert!(html.contains("<p>Text.</p>"));
        let rs = tmp.path().join("a.rs");
        std::fs::write(&rs, "fn a() {}").unwrap();
        s.execute("file.open", &json!({ "path": rs })).unwrap();
        assert!(s.enabled("tools.markdown.exportMarkdownFileTo").is_err());
    }
}
