//! File masks (Find in Files): comma-separated globs on file names.

use crate::fuzzy::fold;

/// A parsed file mask such as `*.rs, *.toml, !*_test.rs`: globs (`*` any run, `?` one char)
/// matched case-insensitively against the whole file name. A `!` prefix excludes. With no
/// include globs every name not excluded matches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileMask {
    include: Vec<Vec<char>>,
    exclude: Vec<Vec<char>>,
}

impl FileMask {
    pub fn parse(mask: &str) -> Self {
        let mut parsed = Self::default();
        for part in mask.split(',').map(str::trim) {
            let (list, glob) = match part.strip_prefix('!') {
                Some(rest) => (&mut parsed.exclude, rest.trim()),
                None => (&mut parsed.include, part),
            };
            if !glob.is_empty() {
                list.push(glob.chars().map(fold).collect());
            }
        }
        parsed
    }

    /// Whether the mask has no globs at all (matches every name).
    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    pub fn matches(&self, file_name: &str) -> bool {
        if self.is_empty() {
            return true;
        }
        let name: Vec<char> = file_name.chars().map(fold).collect();
        if self.exclude.iter().any(|g| glob_match(g, &name)) {
            return false;
        }
        self.include.is_empty() || self.include.iter().any(|g| glob_match(g, &name))
    }
}

/// Wildcard match with single-star backtracking: linear for one `*`, O(len × len) worst case.
fn glob_match(glob: &[char], text: &[char]) -> bool {
    let (mut g, mut t) = (0usize, 0usize);
    // Position after the last `*` seen, and the text position it currently absorbs up to.
    let mut star: Option<(usize, usize)> = None;
    while let Some(&tc) = text.get(t) {
        match glob.get(g) {
            Some('*') => {
                star = Some((g.saturating_add(1), t));
                g = g.saturating_add(1);
            }
            Some(&gc) if gc == '?' || gc == tc => {
                g = g.saturating_add(1);
                t = t.saturating_add(1);
            }
            _ => match star {
                Some((after_star, absorbed)) => {
                    let absorbed = absorbed.saturating_add(1);
                    star = Some((after_star, absorbed));
                    g = after_star;
                    t = absorbed;
                }
                None => return false,
            },
        }
    }
    glob.get(g..).is_some_and(|rest| rest.iter().all(|&c| c == '*'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        let m = |mask: &str, name: &str| FileMask::parse(mask).matches(name);
        assert!(m("*.rs", "main.rs"));
        assert!(!m("*.rs", "main.rsx"));
        assert!(m("*.RS", "Main.rs"));
        assert!(m("*.rs, *.toml", "Cargo.toml"));
        assert!(m("Cargo.toml", "cargo.TOML"));
        assert!(!m("Cargo.toml", "Cargo.toml.bak"));
        assert!(m("?ain.rs", "main.rs"));
        assert!(!m("?ain.rs", "ain.rs"));
        assert!(m("*a*b*c*", "xxaxxbxxcxx"));
        assert!(!m("*a*b*c*", "xxcxxbxxaxx"));
        assert!(m("**", ""));
        assert!(m("*", "anything"));
        assert!(m("日*.md", "日本語.md"));
    }

    #[test]
    fn excludes() {
        let mask = FileMask::parse("*.rs, !*_test.rs, ,!");
        assert!(mask.matches("lib.rs"));
        assert!(!mask.matches("lib_test.rs"));
        assert!(!mask.matches("README.md"));
        let only_excludes = FileMask::parse("!*.lock");
        assert!(only_excludes.matches("main.rs"));
        assert!(!only_excludes.matches("Cargo.lock"));
        assert!(FileMask::parse(" , ").is_empty());
        assert!(FileMask::parse("").matches("x"));
    }

    #[test]
    fn pathological_globs_terminate() {
        let glob: Vec<char> = "*a".repeat(50).chars().collect();
        let text: Vec<char> = "a".repeat(49).chars().chain(['b']).collect();
        assert!(!glob_match(&glob, &text));
    }
}
