//! Link detection in terminal rows: URLs and `path:line:col` file locations.

use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum LinkTarget {
    Url(String),
    /// A file path as printed, with 1-based line and column when present.
    File {
        path: String,
        line: Option<usize>,
        column: Option<usize>,
    },
}

/// A link in a row of text: `[start, end)` in chars.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Link {
    pub start: usize,
    pub end: usize,
    pub target: LinkTarget,
}

fn url_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\b(?:https?|file|ftp)://[^\s<>"'`]+"#).ok()).as_ref()
}

fn path_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        // An optional `~`/`.`/`..` prefix, directories, and a file name with an extension, then
        // an optional `:line` and `:col` (rustc, gcc, pytest, tsc style), or `(line,col)` (MSBuild).
        Regex::new(
            r"(?P<path>(?:~|\.{1,2})?/?(?:[\w.@+\-]+/)*[\w@+\-][\w.@+\-]*\.[A-Za-z0-9_]+)(?::(?P<line>\d+)(?::(?P<col>\d+))?|\((?P<pline>\d+)(?:,(?P<pcol>\d+))?\))?",
        )
        .ok()
    })
    .as_ref()
}

fn trim_url(url: &str) -> &str {
    let mut end = url.len();
    while let Some(c) = url.get(..end).and_then(|s| s.chars().next_back()) {
        let unbalanced_paren = c == ')' && {
            let s = url.get(..end).unwrap_or("");
            s.matches('(').count() < s.matches(')').count()
        };
        if matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ']' | '}' | '\'' | '"') || unbalanced_paren {
            end -= c.len_utf8();
        } else {
            break;
        }
    }
    url.get(..end).unwrap_or(url)
}

fn char_index(s: &str, byte: usize) -> usize {
    s.get(..byte).map_or(0, |p| p.chars().count())
}

/// Find URLs and file locations in a line of terminal text.
pub fn find_links(line: &str) -> Vec<Link> {
    let mut out: Vec<Link> = Vec::new();
    let mut taken: Vec<(usize, usize)> = Vec::new();
    if let Some(re) = url_re() {
        for m in re.find_iter(line) {
            let url = trim_url(m.as_str());
            if url.len() <= "http://".len() {
                continue;
            }
            let (b0, b1) = (m.start(), m.start() + url.len());
            taken.push((b0, b1));
            out.push(Link {
                start: char_index(line, b0),
                end: char_index(line, b1),
                target: LinkTarget::Url(url.to_string()),
            });
        }
    }
    if let Some(re) = path_re() {
        for c in re.captures_iter(line) {
            let (Some(whole), Some(path)) = (c.get(0), c.name("path")) else { continue };
            if taken.iter().any(|(a, b)| whole.start() < *b && whole.end() > *a) {
                continue;
            }
            // `1.5` or `v0.1.0` are not paths: require a directory part or a letter extension.
            let p = path.as_str();
            let ext = p.rsplit('.').next().unwrap_or("");
            if !p.contains('/') && !ext.chars().any(|ch| ch.is_ascii_alphabetic()) {
                continue;
            }
            let num = |name: &str| c.name(name).and_then(|m| m.as_str().parse::<usize>().ok());
            out.push(Link {
                start: char_index(line, whole.start()),
                end: char_index(line, whole.end()),
                target: LinkTarget::File {
                    path: p.to_string(),
                    line: num("line").or_else(|| num("pline")),
                    column: num("col").or_else(|| num("pcol")),
                },
            });
        }
    }
    out.sort_by_key(|l| l.start);
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let l = find_links("see https://example.com/a_(b)/c?x=1, then (http://x.io).");
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].target, LinkTarget::Url("https://example.com/a_(b)/c?x=1".into()));
        assert_eq!(l[0].start, 4);
        assert_eq!(l[1].target, LinkTarget::Url("http://x.io".into()));
    }

    #[test]
    fn file_locations() {
        let l = find_links("  --> src/main.rs:10:5");
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].target, LinkTarget::File { path: "src/main.rs".into(), line: Some(10), column: Some(5) });
        assert_eq!((l[0].start, l[0].end), (6, 22));

        let l = find_links("error in ./lib/a.py:3 and ~/x/Cargo.toml");
        assert_eq!(l[0].target, LinkTarget::File { path: "./lib/a.py".into(), line: Some(3), column: None });
        assert_eq!(l[1].target, LinkTarget::File { path: "~/x/Cargo.toml".into(), line: None, column: None });

        let l = find_links("foo.c(12,3): warning");
        assert_eq!(l[0].target, LinkTarget::File { path: "foo.c".into(), line: Some(12), column: Some(3) });

        assert!(find_links("version 1.5 and v0.1.0").is_empty());
        // Non-ASCII before the link: columns are chars.
        let l = find_links("é → a.rs:1");
        assert_eq!(l[0].start, 4);
    }
}
