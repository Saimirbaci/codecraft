//! Custom inspection scopes: globs over project-relative paths.
//!
//! `src/**/*.rs, !**/generated/**`: comma-separated globs, `!` excludes. `*` and `?` stay inside
//! one path component, `**` spans any number of components, `{a,b}` is not supported (use two
//! globs). A glob without `/` matches the file name anywhere (`*.py`), like `.gitignore`.
//! Matching is case-sensitive on the `/`-separated relative path.

use std::path::Path;

/// Longest pattern accepted (a hostile parameter cannot make matching expensive).
const MAX_PATTERN: usize = 4096;
/// Globs per pattern.
const MAX_GLOBS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Glob {
    /// Path components of the glob (`**` kept as its own component).
    parts: Vec<String>,
    /// No `/` in the glob: match the file name only.
    name_only: bool,
}

/// A parsed custom scope.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathGlob {
    include: Vec<Glob>,
    exclude: Vec<Glob>,
}

impl PathGlob {
    pub fn parse(pattern: &str) -> Result<Self, String> {
        if pattern.len() > MAX_PATTERN {
            return Err("the scope pattern is too long".into());
        }
        let mut out = Self::default();
        for part in pattern.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (exclude, glob) = match part.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, part),
            };
            let glob = glob.trim_start_matches("./").trim_start_matches('/');
            if glob.is_empty() {
                continue;
            }
            let g = Glob {
                name_only: !glob.contains('/'),
                parts: glob.split('/').filter(|c| !c.is_empty()).map(str::to_string).collect(),
            };
            if exclude { out.exclude.push(g) } else { out.include.push(g) }
            if out.include.len() + out.exclude.len() > MAX_GLOBS {
                return Err(format!("at most {MAX_GLOBS} globs in a scope pattern"));
            }
        }
        if out.include.is_empty() && out.exclude.is_empty() {
            return Err("the scope pattern is empty".into());
        }
        Ok(out)
    }

    /// Whether a project-relative path is in the scope.
    pub fn matches(&self, rel: &Path) -> bool {
        let comps: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        let hit = |g: &Glob| {
            if g.name_only {
                comps.last().is_some_and(|name| g.parts.first().is_some_and(|p| component_match(p, name)))
            } else {
                parts_match(&g.parts, &comps)
            }
        };
        if self.exclude.iter().any(hit) {
            return false;
        }
        self.include.is_empty() || self.include.iter().any(hit)
    }
}

/// Component lists with `**` (any number of components, including none). Dynamic programming
/// over (glob suffix, path suffix): O(glob × path) without exponential backtracking.
fn parts_match(glob: &[String], path: &[String]) -> bool {
    let (g, p) = (glob.len(), path.len());
    // ok[j] for the current glob suffix i: glob[i..] matches path[j..].
    let mut next = vec![false; p + 1];
    // The empty glob matches only the empty path.
    if let Some(last) = next.last_mut() {
        *last = true;
    }
    for i in (0..g).rev() {
        let mut cur = vec![false; p + 1];
        let Some(part) = glob.get(i) else { return false };
        if part == "**" {
            // cur[j] = next[j] || cur[j + 1]
            for j in (0..=p).rev() {
                let skip = next.get(j).copied().unwrap_or(false);
                let absorb = j < p && cur.get(j + 1).copied().unwrap_or(false);
                if let Some(c) = cur.get_mut(j) {
                    *c = skip || absorb;
                }
            }
        } else {
            for j in 0..p {
                let ok = path.get(j).is_some_and(|name| component_match(part, name))
                    && next.get(j + 1).copied().unwrap_or(false);
                if let Some(c) = cur.get_mut(j) {
                    *c = ok;
                }
            }
        }
        next = cur;
    }
    next.first().copied().unwrap_or(false)
}

/// One component: `*` any run of chars, `?` one char (backtracking on the last `*`).
fn component_match(glob: &str, text: &str) -> bool {
    let g: Vec<char> = glob.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut gi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match g.get(gi) {
            Some('*') => {
                star = Some((gi + 1, ti));
                gi += 1;
            }
            Some(&c) if c == '?' || Some(&c) == t.get(ti) => {
                gi += 1;
                ti += 1;
            }
            _ => match star {
                Some((after, absorbed)) => {
                    star = Some((after, absorbed + 1));
                    gi = after;
                    ti = absorbed + 1;
                }
                None => return false,
            },
        }
    }
    g.get(gi..).is_some_and(|rest| rest.iter().all(|c| *c == '*'))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn m(pattern: &str, path: &str) -> bool {
        PathGlob::parse(pattern).unwrap().matches(Path::new(path))
    }

    #[test]
    fn globs_over_relative_paths() {
        assert!(m("src/**/*.rs", "src/main.rs"));
        assert!(m("src/**/*.rs", "src/a/b/c.rs"));
        assert!(!m("src/**/*.rs", "tests/a.rs"));
        assert!(!m("src/*.rs", "src/a/b.rs"), "* stays in one component");
        assert!(m("*.py", "pkg/mod/x.py"), "a bare glob matches names anywhere");
        assert!(m("**", "anything/at/all.txt"));
        assert!(m("src/**", "src/x/y.c"));
        assert!(m("a?c.rs", "abc.rs"));
        assert!(!m("a?c.rs", "abbc.rs"));
        assert!(m("src/**/*.rs, !**/gen/**", "src/x.rs"));
        assert!(!m("src/**/*.rs, !**/gen/**", "src/gen/x.rs"));
        assert!(m("!*.md", "src/x.rs"), "only excludes: everything else");
        assert!(!m("!*.md", "README.md"));
        assert!(m("./src/*.rs", "src/a.rs"));
        assert!(m("*é*", "café.txt"));
    }

    #[test]
    fn bad_patterns_are_errors() {
        assert!(PathGlob::parse("").is_err());
        assert!(PathGlob::parse(" , ,").is_err());
        assert!(PathGlob::parse(&"a,".repeat(100)).is_err());
        assert!(PathGlob::parse(&"x".repeat(5000)).is_err());
        // Pathological stars stay fast.
        let g = PathGlob::parse(&format!("{}b", "*a".repeat(30))).unwrap();
        assert!(!g.matches(Path::new(&"a".repeat(200))));
        let deep = PathGlob::parse("**/**/**/**/**/**/x").unwrap();
        assert!(!deep.matches(Path::new(&"d/".repeat(30))));
    }
}
