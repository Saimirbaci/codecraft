//! `git blame --porcelain`: who last changed each line (Git › Current File › Annotate).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{Git, GitError, Result};

const ZERO: &str = "0000000000000000000000000000000000000000";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameCommit {
    pub hash: String,
    pub author: String,
    pub email: String,
    /// Author time, seconds since the epoch.
    pub time: i64,
    pub summary: String,
    /// The line is not committed yet.
    pub uncommitted: bool,
}

impl BlameCommit {
    pub fn short(&self) -> &str {
        self.hash.get(..8).unwrap_or(&self.hash)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blame {
    pub commits: Vec<BlameCommit>,
    /// For each line (0-based) of the blamed text: an index into `commits`.
    pub lines: Vec<usize>,
}

impl Blame {
    pub fn line(&self, line: usize) -> Option<&BlameCommit> {
        self.lines.get(line).and_then(|i| self.commits.get(*i))
    }
}

/// Blame `path` (repository-relative). With `contents`, the given text (an editor's unsaved
/// buffer) is blamed instead of the file on disk, so lines match what the user sees.
pub fn blame(git: &Git, path: &str, contents: Option<&str>) -> Result<Blame> {
    let mut args = vec!["blame", "--porcelain"];
    if contents.is_some() {
        args.extend(["--contents", "-"]);
    }
    args.extend(["--", path]);
    let mut cmd = git.cmd(args).read_only();
    if let Some(c) = contents {
        cmd = cmd.stdin(c.as_bytes().to_vec());
    }
    parse(&cmd.run()?.stdout)
}

/// Parse porcelain blame output.
pub fn parse(out: &[u8]) -> Result<Blame> {
    let text = String::from_utf8_lossy(out);
    let mut blame = Blame::default();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut current: Option<usize> = None;
    let mut pending_line: Option<usize> = None;
    for line in text.split('\n') {
        if let Some(_content) = line.strip_prefix('\t') {
            let (Some(c), Some(final_line)) = (current, pending_line.take()) else {
                return Err(GitError::Parse("blame content without a header".into()));
            };
            if final_line >= 1_000_000 {
                return Err(GitError::Parse("blame line number out of range".into()));
            }
            if blame.lines.len() < final_line {
                blame.lines.resize(final_line, c);
            }
            if let Some(slot) = blame.lines.get_mut(final_line.saturating_sub(1)) {
                *slot = c;
            } else {
                blame.lines.push(c);
            }
            continue;
        }
        let mut parts = line.split(' ');
        let first = parts.next().unwrap_or("");
        if first.len() == 40 && first.chars().all(|c| c.is_ascii_hexdigit()) {
            let _orig = parts.next();
            let final_line: usize = parts
                .next()
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| GitError::Parse(format!("blame header {line:?}")))?;
            let i = *index.entry(first.to_string()).or_insert_with(|| {
                blame.commits.push(BlameCommit {
                    hash: first.to_string(),
                    uncommitted: first == ZERO,
                    ..BlameCommit::default()
                });
                blame.commits.len() - 1
            });
            current = Some(i);
            pending_line = Some(final_line);
            continue;
        }
        let Some(c) = current.and_then(|i| blame.commits.get_mut(i)) else { continue };
        let value = line.split_once(' ').map(|(_, v)| v).unwrap_or("");
        match first {
            "author" => c.author = value.to_string(),
            "author-mail" => c.email = value.trim_start_matches('<').trim_end_matches('>').to_string(),
            "author-time" => c.time = value.trim().parse().unwrap_or(0),
            "summary" => c.summary = value.to_string(),
            _ => {}
        }
    }
    Ok(blame)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::test_util::{Sandbox, git_available, write};

    #[test]
    fn blames_committed_and_unsaved_lines() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (root, git) = sb.repo("r", &[("a.txt", "one\ntwo\n")]);
        write(&root, "a.txt", "one\ntwo\nthree\n");
        git.run(["commit", "-qam", "add three"]).unwrap();
        let b = blame(&git, "a.txt", None).unwrap();
        assert_eq!(b.lines.len(), 3);
        assert_eq!(b.line(0).unwrap().summary, "initial");
        assert_eq!(b.line(2).unwrap().summary, "add three");
        assert_eq!(b.line(2).unwrap().author, "Test User");
        assert_eq!(b.line(2).unwrap().email, "test@example.com");
        assert!(b.line(2).unwrap().time > 0);
        // An unsaved buffer: the new line is not committed yet.
        let b = blame(&git, "a.txt", Some("one\nnew\ntwo\nthree\n")).unwrap();
        assert_eq!(b.lines.len(), 4);
        assert!(b.line(1).unwrap().uncommitted);
        assert!(!b.line(3).unwrap().uncommitted);
        assert!(blame(&git, "missing.txt", None).is_err());
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(parse(b"\tcontent first\n").is_err());
        let h = "a".repeat(40);
        assert!(parse(format!("{h} 1\n").as_bytes()).is_err());
        assert!(parse(format!("{h} 1 99999999\n\tx\n").as_bytes()).is_err());
        assert_eq!(parse(b"").unwrap().lines.len(), 0);
    }
}
