//! History: paged `git log` with refs and parents, a lane layout for the commit graph, commit
//! details with changed files, and file contents at a revision.

use serde::{Deserialize, Serialize};

use crate::status::FileState;
use crate::{Git, GitError, Result, pathspec};

/// Blobs larger than this are not loaded for diffs.
pub const MAX_BLOB: usize = 8 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RefKind {
    /// `HEAD` (detached) or the branch `HEAD` points to.
    Head,
    LocalBranch,
    RemoteBranch,
    Tag,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefLabel {
    pub name: String,
    pub kind: RefKind,
    /// The current branch (`HEAD -> main`).
    pub current: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub hash: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    /// Author time, seconds since the epoch.
    pub time: i64,
    pub refs: Vec<RefLabel>,
    pub subject: String,
}

impl Commit {
    pub fn short(&self) -> &str {
        self.hash.get(..8).unwrap_or(&self.hash)
    }
}

/// Which commits to list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogQuery {
    pub skip: usize,
    pub limit: usize,
    /// A branch or revision; `None` = all refs (`--all`), like the Git tool window's default.
    pub branch: Option<String>,
    /// Only commits touching this repository-relative path.
    pub path: Option<String>,
    /// Matches the message (case-insensitive), or a hash prefix.
    pub text: Option<String>,
    /// Matches the author's name or email (case-insensitive).
    pub author: Option<String>,
}

const LOG_FORMAT: &str = "--format=%x1e%H%x00%P%x00%an%x00%ae%x00%at%x00%D%x00%s";

/// One page of history.
pub fn log(git: &Git, q: &LogQuery) -> Result<Vec<Commit>> {
    let mut args: Vec<String> = vec![
        "log".into(),
        LOG_FORMAT.into(),
        "--decorate=full".into(),
        "--date-order".into(),
        format!("--skip={}", q.skip),
        format!("--max-count={}", q.limit.clamp(1, 10_000)),
    ];
    let text = q.text.as_deref().map(str::trim).filter(|t| !t.is_empty());
    if let Some(t) = text {
        // A hash prefix shows that commit; otherwise match message or author.
        if t.len() >= 4
            && t.len() <= 40
            && t.chars().all(|c| c.is_ascii_hexdigit())
            && let Ok(full) = git.read_text(["rev-parse", "--verify", "--quiet", &format!("{t}^{{commit}}")])
            && !full.trim().is_empty()
        {
            args.push("--no-walk".into());
            args.push(full.trim().to_string());
            return log_records(&git.read(args)?);
        }
        args.push("--regexp-ignore-case".into());
        args.push("--fixed-strings".into());
        args.push(format!("--grep={t}"));
    }
    if let Some(a) = q.author.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        args.push("--regexp-ignore-case".into());
        args.push("--fixed-strings".into());
        args.push(format!("--author={a}"));
    }
    match &q.branch {
        Some(b) => args.push(b.clone()),
        None => args.push("--all".into()),
    }
    args.push("--".into());
    if let Some(p) = &q.path {
        args.push(pathspec(p));
    }
    match git.read(&args) {
        Ok(out) => log_records(&out),
        // An empty repository has no history yet.
        Err(GitError::Failed { message, .. })
            if message.contains("does not have any commits") || message.contains("bad default revision") =>
        {
            Ok(Vec::new())
        }
        Err(e) => Err(e),
    }
}

pub(crate) fn log_records(out: &[u8]) -> Result<Vec<Commit>> {
    let text = String::from_utf8_lossy(out);
    let mut commits = Vec::new();
    for rec in text.split('\x1e') {
        let rec = rec.trim_end_matches('\n');
        if rec.is_empty() {
            continue;
        }
        let f: Vec<&str> = rec.splitn(7, '\0').collect();
        let [hash, parents, author, email, time, refs, subject] = f.as_slice() else {
            return Err(GitError::Parse(format!("log record {:?}", rec.chars().take(80).collect::<String>())));
        };
        commits.push(Commit {
            hash: (*hash).to_string(),
            parents: parents.split_whitespace().map(str::to_string).collect(),
            author: (*author).to_string(),
            email: (*email).to_string(),
            time: time.trim().parse().unwrap_or(0),
            refs: parse_refs(refs),
            subject: (*subject).to_string(),
        });
    }
    Ok(commits)
}

/// Parse `%D` with `--decorate=full`: `HEAD -> refs/heads/main, refs/remotes/origin/main, tag: refs/tags/v1`.
pub fn parse_refs(d: &str) -> Vec<RefLabel> {
    let mut out = Vec::new();
    for part in d.split(", ").map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(target) = part.strip_prefix("HEAD -> ") {
            let (name, kind) = classify(target);
            out.push(RefLabel { name, kind, current: true });
        } else if part == "HEAD" {
            out.push(RefLabel { name: "HEAD".into(), kind: RefKind::Head, current: true });
        } else if let Some(tag) = part.strip_prefix("tag: ") {
            out.push(RefLabel {
                name: tag.strip_prefix("refs/tags/").unwrap_or(tag).to_string(),
                kind: RefKind::Tag,
                current: false,
            });
        } else {
            let (name, kind) = classify(part);
            // `origin/HEAD` adds nothing next to `origin/main`.
            if kind == RefKind::RemoteBranch && name.ends_with("/HEAD") {
                continue;
            }
            out.push(RefLabel { name, kind, current: false });
        }
    }
    out
}

fn classify(r: &str) -> (String, RefKind) {
    if let Some(n) = r.strip_prefix("refs/heads/") {
        (n.to_string(), RefKind::LocalBranch)
    } else if let Some(n) = r.strip_prefix("refs/remotes/") {
        (n.to_string(), RefKind::RemoteBranch)
    } else if let Some(n) = r.strip_prefix("refs/tags/") {
        (n.to_string(), RefKind::Tag)
    } else {
        (r.to_string(), RefKind::Other)
    }
}

// ---------------------------------------------------------------- graph

/// How one row of the commit graph is drawn. Lanes are columns; every segment goes from the
/// row's top edge to its middle (`top`) or from the middle to its bottom edge (`bottom`), from
/// lane `.0` to lane `.1`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphRow {
    /// The commit's lane.
    pub lane: usize,
    pub top: Vec<(usize, usize)>,
    pub bottom: Vec<(usize, usize)>,
    /// Number of lanes in use at this row (for the column width).
    pub width: usize,
}

/// Lay out the commit graph of a page of history (commits in log order, children first).
pub fn graph(commits: &[Commit]) -> Vec<GraphRow> {
    let mut lanes: Vec<Option<&str>> = Vec::new();
    let mut rows = Vec::with_capacity(commits.len());
    for c in commits {
        let mut row = GraphRow::default();
        let lane = match lanes.iter().position(|l| *l == Some(c.hash.as_str())) {
            Some(i) => i,
            None => match lanes.iter().position(Option::is_none) {
                Some(i) => i,
                None => {
                    lanes.push(None);
                    lanes.len() - 1
                }
            },
        };
        row.lane = lane;
        // Top half: lanes waiting for this commit join it; the others pass through.
        for (j, l) in lanes.iter().enumerate() {
            match l {
                Some(h) if *h == c.hash => row.top.push((j, lane)),
                Some(_) => row.top.push((j, j)),
                None => {}
            }
        }
        for l in lanes.iter_mut() {
            if *l == Some(c.hash.as_str()) {
                *l = None;
            }
        }
        // Bottom half: the first parent continues in this lane, others go to their lanes.
        let mut targets: Vec<usize> = Vec::new();
        let mut moved: Vec<usize> = Vec::new();
        for (k, p) in c.parents.iter().enumerate() {
            let existing = lanes.iter().position(|l| *l == Some(p.as_str()));
            let t = if k == 0 && existing.is_none_or(|j| j > lane) {
                // The first parent continues in this lane; a lane further right already waiting
                // for it joins here, so first-parent chains stay on the left.
                if let Some(j) = existing {
                    if let Some(slot) = lanes.get_mut(j) {
                        *slot = None;
                    }
                    moved.push(j);
                    row.bottom.push((j, lane));
                }
                if let Some(slot) = lanes.get_mut(lane) {
                    *slot = Some(p.as_str());
                }
                lane
            } else if let Some(i) = existing {
                i
            } else {
                match lanes.iter().position(Option::is_none) {
                    Some(i) => {
                        if let Some(slot) = lanes.get_mut(i) {
                            *slot = Some(p.as_str());
                        }
                        i
                    }
                    None => {
                        lanes.push(Some(p.as_str()));
                        lanes.len() - 1
                    }
                }
            };
            if !targets.contains(&t) {
                targets.push(t);
                row.bottom.push((lane, t));
            }
        }
        for (j, l) in lanes.iter().enumerate() {
            if l.is_some() && j != lane && !targets.contains(&j) && !moved.contains(&j) {
                row.bottom.push((j, j));
            }
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        let used = row.top.iter().chain(&row.bottom).flat_map(|(a, b)| [*a, *b]).max().unwrap_or(0);
        row.width = used.max(lane) + 1;
        rows.push(row);
    }
    rows
}

// ---------------------------------------------------------------- commit details

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub orig_path: Option<String>,
    pub state: FileState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitDetails {
    pub hash: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    pub time: i64,
    pub committer: String,
    pub commit_time: i64,
    pub refs: Vec<RefLabel>,
    /// The full message.
    pub message: String,
    /// Changes against the first parent (everything for a root commit).
    pub files: Vec<ChangedFile>,
}

/// A commit's metadata and changed files.
pub fn show_commit(git: &Git, rev: &str) -> Result<CommitDetails> {
    let out = git.read_text([
        "show",
        "-s",
        "--decorate=full",
        "--format=%H%x00%P%x00%an%x00%ae%x00%at%x00%cn%x00%ct%x00%D%x00%B",
        rev,
        "--",
    ])?;
    let f: Vec<&str> = out.splitn(9, '\0').collect();
    let [hash, parents, author, email, time, committer, ctime, refs, message] = f.as_slice() else {
        return Err(GitError::Parse(format!("commit {rev}")));
    };
    let parents: Vec<String> = parents.split_whitespace().map(str::to_string).collect();
    let files = changed_files(git, parents.first().map(String::as_str), hash)?;
    Ok(CommitDetails {
        hash: (*hash).to_string(),
        parents,
        author: (*author).to_string(),
        email: (*email).to_string(),
        time: time.trim().parse().unwrap_or(0),
        committer: (*committer).to_string(),
        commit_time: ctime.trim().parse().unwrap_or(0),
        refs: parse_refs(refs),
        message: message.trim_end().to_string(),
        files,
    })
}

/// Files changed between `from` (or nothing, for a root commit) and `to`.
pub fn changed_files(git: &Git, from: Option<&str>, to: &str) -> Result<Vec<ChangedFile>> {
    let out = match from {
        Some(p) => git.read(["diff", "--no-ext-diff", "--name-status", "-M", "-z", p, to, "--"])?,
        None => git.read(["diff-tree", "--root", "--no-commit-id", "-r", "-M", "--name-status", "-z", to, "--"])?,
    };
    Ok(parse_name_status(&out))
}

/// Parse `--name-status -z`.
pub fn parse_name_status(out: &[u8]) -> Vec<ChangedFile> {
    let text = String::from_utf8_lossy(out);
    let mut it = text.split('\0').filter(|s| !s.is_empty());
    let mut files = Vec::new();
    while let Some(code) = it.next() {
        let c = code.chars().next().unwrap_or('M');
        if matches!(c, 'R' | 'C') {
            let (Some(old), Some(new)) = (it.next(), it.next()) else { break };
            files.push(ChangedFile {
                path: new.to_string(),
                orig_path: Some(old.to_string()),
                state: FileState::Renamed,
            });
        } else {
            let Some(path) = it.next() else { break };
            let state = match c {
                'A' => FileState::Added,
                'D' => FileState::Deleted,
                'U' => FileState::Conflicted,
                _ => FileState::Modified,
            };
            files.push(ChangedFile { path: path.to_string(), orig_path: None, state });
        }
    }
    files
}

/// A file's bytes at a revision (`rev` empty = the index). `None` if it doesn't exist there.
pub fn file_at(git: &Git, rev: &str, path: &str) -> Result<Option<Vec<u8>>> {
    let spec = format!("{rev}:{path}");
    let size = match git.cmd(["cat-file", "-s", &spec]).read_only().run() {
        Ok(o) => o.text().trim().parse::<usize>().unwrap_or(0),
        Err(GitError::Failed { .. }) => return Ok(None),
        Err(e) => return Err(e),
    };
    if size > MAX_BLOB {
        return Err(GitError::TooLarge(format!("{path} is too large to compare ({} MB)", size >> 20)));
    }
    match git.cmd(["cat-file", "blob", &spec]).read_only().run() {
        Ok(o) => Ok(Some(o.stdout)),
        Err(GitError::Failed { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Text of a file at a revision (`None`: missing or binary).
pub fn text_at(git: &Git, rev: &str, path: &str) -> Result<Option<String>> {
    Ok(file_at(git, rev, path)?.and_then(|b| if b.contains(&0) { None } else { String::from_utf8(b).ok() }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::test_util::{Sandbox, git_available, write};

    fn c(hash: &str, parents: &[&str]) -> Commit {
        Commit {
            hash: hash.into(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            author: String::new(),
            email: String::new(),
            time: 0,
            refs: Vec::new(),
            subject: String::new(),
        }
    }

    #[test]
    fn graph_of_a_merge() {
        // m merges b into a; both come from r.
        let commits = [c("m", &["a", "b"]), c("b", &["r"]), c("a", &["r"]), c("r", &[])];
        let g = graph(&commits);
        assert_eq!(g[0].lane, 0);
        assert_eq!(g[0].bottom, vec![(0, 0), (0, 1)]);
        assert_eq!(g[1].lane, 1);
        assert_eq!(g[1].top, vec![(0, 0), (1, 1)]);
        // a's first parent r was waiting in b's lane: that lane joins a's below it.
        assert_eq!(g[2].lane, 0);
        assert!(g[2].bottom.contains(&(1, 0)), "{g:?}");
        assert_eq!(g[3].lane, 0, "{g:?}");
        assert_eq!(g[3].top, vec![(0, 0)]);
        assert!(g.iter().all(|r| r.width >= 1 && r.width <= 2));
        // Garbage-free on odd input.
        assert_eq!(graph(&[c("x", &["x"])]).len(), 1);
        assert!(graph(&[]).is_empty());
    }

    #[test]
    fn refs_and_name_status() {
        let r = parse_refs(
            "HEAD -> refs/heads/main, refs/remotes/origin/main, refs/remotes/origin/HEAD, tag: refs/tags/v1",
        );
        assert_eq!(r.len(), 3);
        assert_eq!(r[0], RefLabel { name: "main".into(), kind: RefKind::LocalBranch, current: true });
        assert_eq!(r[1].kind, RefKind::RemoteBranch);
        assert_eq!(r[2], RefLabel { name: "v1".into(), kind: RefKind::Tag, current: false });
        let f = parse_name_status(b"M\0a b.rs\0R087\0old\0new\0A\0x\0D");
        assert_eq!(f.len(), 3);
        assert_eq!(f[1].orig_path.as_deref(), Some("old"));
        assert_eq!(f[2].state, FileState::Added);
    }

    #[test]
    fn log_details_and_contents() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (root, git) = sb.repo("r", &[("a.txt", "one\n")]);
        write(&root, "a.txt", "two\n");
        write(&root, "b.txt", "bee\n");
        git.run(["add", "-A"]).unwrap();
        git.run(["commit", "-q", "-m", "Second change\n\nWith a body"]).unwrap();
        git.run(["tag", "v1"]).unwrap();
        let all = log(&git, &LogQuery { limit: 50, ..Default::default() }).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].subject, "Second change");
        assert_eq!(all[0].author, "Test User");
        assert!(all[0].refs.iter().any(|r| r.kind == RefKind::Tag && r.name == "v1"));
        assert!(all[0].refs.iter().any(|r| r.current && r.name == "main"));
        assert_eq!(all[0].parents, vec![all[1].hash.clone()]);
        // Paging, path and text filters.
        let page = log(&git, &LogQuery { skip: 1, limit: 1, ..Default::default() }).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].subject, "initial");
        let by_path = log(&git, &LogQuery { limit: 50, path: Some("b.txt".into()), ..Default::default() }).unwrap();
        assert_eq!(by_path.len(), 1);
        let by_text = log(&git, &LogQuery { limit: 50, text: Some("SECOND".into()), ..Default::default() }).unwrap();
        assert_eq!(by_text.len(), 1);
        let by_author =
            log(&git, &LogQuery { limit: 50, author: Some("test user".into()), ..Default::default() }).unwrap();
        assert_eq!(by_author.len(), 2);
        let nobody = log(&git, &LogQuery { limit: 50, author: Some("nobody".into()), ..Default::default() }).unwrap();
        assert!(nobody.is_empty());
        let by_hash =
            log(&git, &LogQuery { limit: 50, text: Some(all[1].hash[..7].into()), ..Default::default() }).unwrap();
        assert_eq!(by_hash.len(), 1);
        assert_eq!(by_hash[0].subject, "initial");
        let d = show_commit(&git, &all[0].hash).unwrap();
        assert_eq!(d.message, "Second change\n\nWith a body");
        assert_eq!(d.files.len(), 2);
        assert!(d.files.iter().any(|f| f.path == "b.txt" && f.state == FileState::Added));
        let root_details = show_commit(&git, &all[1].hash).unwrap();
        assert_eq!(root_details.files.len(), 1);
        assert_eq!(text_at(&git, "HEAD", "a.txt").unwrap().as_deref(), Some("two\n"));
        assert_eq!(text_at(&git, "HEAD~1", "a.txt").unwrap().as_deref(), Some("one\n"));
        assert_eq!(text_at(&git, "HEAD", "missing").unwrap(), None);
        assert_eq!(text_at(&git, "", "b.txt").unwrap().as_deref(), Some("bee\n"));
        // An empty repository has no history (and no error).
        let (_, empty) = sb.repo("empty", &[]);
        assert!(log(&empty, &LogQuery { limit: 10, ..Default::default() }).unwrap().is_empty());
        let _ = log(&empty, &LogQuery { limit: 10, branch: Some("HEAD".into()), ..Default::default() });
    }
}
