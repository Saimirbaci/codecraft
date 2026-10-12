//! Branches, tags, remotes and stashes (listing; the operations are in `ops`).

use serde::{Deserialize, Serialize};

use crate::{Git, GitError, Result};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    /// Short name: `main`, `origin/main`.
    pub name: String,
    pub remote: bool,
    /// Checked out in this work tree.
    pub current: bool,
    /// The upstream of a local branch (`origin/main`).
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    /// The upstream branch was deleted on the remote.
    pub gone: bool,
    pub hash: String,
    pub subject: String,
    /// Committer time of the tip, seconds since the epoch.
    pub time: i64,
}

impl Branch {
    /// For a remote branch `origin/feature/x`: (`origin`, `feature/x`).
    pub fn remote_parts(&self) -> Option<(&str, &str)> {
        if self.remote { self.name.split_once('/') } else { None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub name: String,
    pub hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub name: String,
    pub fetch_url: String,
    pub push_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stash {
    /// 0 = the newest (`stash@{0}`).
    pub index: usize,
    pub hash: String,
    pub time: i64,
    /// e.g. `On main: message` / `WIP on main: abc123 subject`.
    pub message: String,
}

impl Stash {
    pub fn reference(&self) -> String {
        format!("stash@{{{}}}", self.index)
    }
}

/// Local and remote branches, most recently committed first within each group.
pub fn branches(git: &Git) -> Result<Vec<Branch>> {
    let out = git.read_text([
        "for-each-ref",
        "--sort=-committerdate",
        "--format=%(refname)%00%(objectname)%00%(upstream:short)%00%(upstream:track,nobracket)%00%(HEAD)%00%(committerdate:unix)%00%(subject)",
        "refs/heads",
        "refs/remotes",
    ])?;
    let mut list = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.splitn(7, '\0').collect();
        let [refname, hash, upstream, track, head, time, subject] = f.as_slice() else {
            return Err(GitError::Parse(format!("ref {:?}", line.chars().take(80).collect::<String>())));
        };
        let (name, remote) = if let Some(n) = refname.strip_prefix("refs/heads/") {
            (n, false)
        } else if let Some(n) = refname.strip_prefix("refs/remotes/") {
            (n, true)
        } else {
            continue;
        };
        if remote && name.ends_with("/HEAD") {
            continue;
        }
        let (mut ahead, mut behind) = (0, 0);
        for part in track.split(", ") {
            if let Some(n) = part.strip_prefix("ahead ") {
                ahead = n.trim().parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                behind = n.trim().parse().unwrap_or(0);
            }
        }
        list.push(Branch {
            name: name.to_string(),
            remote,
            current: *head == "*",
            upstream: (!upstream.is_empty()).then(|| (*upstream).to_string()),
            ahead,
            behind,
            gone: *track == "gone",
            hash: (*hash).to_string(),
            subject: (*subject).to_string(),
            time: time.trim().parse().unwrap_or(0),
        });
    }
    list.sort_by_key(|b| b.remote);
    Ok(list)
}

pub fn tags(git: &Git) -> Result<Vec<Tag>> {
    let out = git.read_text([
        "for-each-ref",
        "--sort=-creatordate",
        "--format=%(refname:short)%00%(objectname)",
        "refs/tags",
    ])?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once('\0'))
        .map(|(n, h)| Tag { name: n.to_string(), hash: h.to_string() })
        .collect())
}

pub fn remotes(git: &Git) -> Result<Vec<Remote>> {
    let out = git.read_text(["remote", "-v"])?;
    let mut list: Vec<Remote> = Vec::new();
    for line in out.lines() {
        let mut it = line.split_whitespace();
        let (Some(name), Some(url), kind) = (it.next(), it.next(), it.next()) else { continue };
        let i = match list.iter().position(|r| r.name == name) {
            Some(i) => i,
            None => {
                list.push(Remote { name: name.to_string(), fetch_url: String::new(), push_url: String::new() });
                list.len() - 1
            }
        };
        if let Some(r) = list.get_mut(i) {
            if kind == Some("(push)") {
                r.push_url = url.to_string();
            } else {
                r.fetch_url = url.to_string();
            }
        }
    }
    Ok(list)
}

pub fn stashes(git: &Git) -> Result<Vec<Stash>> {
    let out = git.read_text(["stash", "list", "--format=%gd%x00%H%x00%ct%x00%gs"])?;
    let mut list = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.splitn(4, '\0').collect();
        let [refname, hash, time, message] = f.as_slice() else { continue };
        let index = refname
            .strip_prefix("stash@{")
            .and_then(|s| s.strip_suffix('}'))
            .and_then(|n| n.parse().ok())
            .unwrap_or(list.len());
        list.push(Stash {
            index,
            hash: (*hash).to_string(),
            time: time.trim().parse().unwrap_or(0),
            message: (*message).to_string(),
        });
    }
    Ok(list)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::test_util::{Sandbox, git_available};

    #[test]
    fn lists_refs() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (_, git) = sb.repo("r", &[("a.txt", "a\n")]);
        git.run(["branch", "feature/x"]).unwrap();
        git.run(["tag", "v1"]).unwrap();
        git.run(["remote", "add", "origin", "https://example.invalid/r.git"]).unwrap();
        let b = branches(&git).unwrap();
        assert_eq!(b.len(), 2);
        assert!(b.iter().any(|b| b.name == "main" && b.current && !b.remote));
        assert!(b.iter().any(|b| b.name == "feature/x" && !b.current && b.subject == "initial"));
        assert_eq!(tags(&git).unwrap()[0].name, "v1");
        let r = remotes(&git).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].push_url, "https://example.invalid/r.git");
        assert!(stashes(&git).unwrap().is_empty());
        let rb = Branch {
            name: "origin/feature/x".into(),
            remote: true,
            current: false,
            upstream: None,
            ahead: 0,
            behind: 0,
            gone: false,
            hash: String::new(),
            subject: String::new(),
            time: 0,
        };
        assert_eq!(rb.remote_parts(), Some(("origin", "feature/x")));
    }
}
