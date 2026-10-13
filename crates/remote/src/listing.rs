//! Remote directory listings: `LC_ALL=C ls -lAn` output (GNU and BSD), one entry per line:
//! `perms links uid gid size month day time-or-year name` (devices show `major, minor`).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
    Other,
}

/// One remote directory entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    /// As `ls` printed it (`Oct 13 12:00` or `Oct 13  2025`).
    pub modified: String,
    pub permissions: String,
    pub link_target: Option<String>,
}

impl RemoteEntry {
    /// Directories (and links, which may point at one) open by browsing.
    pub fn is_dir_like(&self) -> bool {
        matches!(self.kind, EntryKind::Dir | EntryKind::Symlink)
    }
}

/// Whitespace-separated tokens with their byte offsets.
fn tokens(line: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in line.char_indices() {
        if c == ' ' || c == '\t' {
            if let Some(s) = start.take()
                && let Some(t) = line.get(s..i)
            {
                out.push((s, t));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start
        && let Some(t) = line.get(s..)
    {
        out.push((s, t));
    }
    out
}

/// Parse `ls -lAn` output; `.`/`..` and unparsable lines are skipped. Directories first, then by
/// name.
pub fn parse_ls(output: &str) -> Vec<RemoteEntry> {
    let mut out: Vec<RemoteEntry> = output.lines().filter_map(parse_line).collect();
    out.sort_by(|a, b| (a.kind != EntryKind::Dir, &a.name).cmp(&(b.kind != EntryKind::Dir, &b.name)));
    out
}

/// Output of [`crate::command::pwd_ls_command`]: the absolute directory (first line) and its
/// entries.
pub fn parse_pwd_ls(output: &str) -> (Option<String>, Vec<RemoteEntry>) {
    let pwd =
        output.lines().next().map(|l| l.trim_end_matches('\r')).filter(|l| l.starts_with('/')).map(str::to_string);
    (pwd, parse_ls(output))
}

fn parse_line(line: &str) -> Option<RemoteEntry> {
    let line = line.trim_end_matches('\r');
    let toks = tokens(line);
    let (_, perms) = *toks.first()?;
    let kind = match perms.chars().next()? {
        '-' => EntryKind::File,
        'd' => EntryKind::Dir,
        'l' => EntryKind::Symlink,
        'b' | 'c' | 'p' | 's' | 'D' => EntryKind::Other,
        _ => return None,
    };
    if perms.chars().count() < 10 {
        return None;
    }
    // Devices print `major, minor` where others print the size.
    let device = toks.get(4).is_some_and(|(_, t)| t.ends_with(','));
    let size_idx = if device { 5 } else { 4 };
    let size = if device { 0 } else { toks.get(size_idx)?.1.parse().ok()? };
    let date: Vec<&str> = toks.get(size_idx + 1..size_idx + 4)?.iter().map(|(_, t)| *t).collect();
    let (name_start, _) = *toks.get(size_idx + 4)?;
    let mut name = line.get(name_start..)?.to_string();
    let mut link_target = None;
    if kind == EntryKind::Symlink
        && let Some((n, t)) = name.split_once(" -> ")
    {
        link_target = Some(t.to_string());
        name = n.to_string();
    }
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return None;
    }
    Some(RemoteEntry { name, kind, size, modified: date.join(" "), permissions: perms.to_string(), link_target })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnu_and_bsd_listings() {
        let out = "total 24\n\
drwxr-xr-x  2 1000 1000 4096 Oct 13 12:00 .\n\
drwxr-xr-x  9 1000 1000 4096 Oct  1  2025 ..\n\
-rw-r--r--  1 1000 1000  220 Oct 13 12:00 my file.txt\n\
drwxr-xr-x  3 1000 1000 4096 Jan  2  2024 src\n\
lrwxrwxrwx  1 1000 1000    7 Oct 13 12:00 link -> target\n\
-rw-r--r--@ 1 501  20   1024 Oct 13 12:00 mac.txt\n\
crw-rw-rw-  1 0    0    1,   3 Oct 13 12:00 null\n\
garbage line\n\
-rw-r--r--  1 1000 1000 notanumber Oct 13 12:00 bad\n";
        let e = parse_ls(out);
        let names: Vec<&str> = e.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["src", "link", "mac.txt", "my file.txt", "null"]);
        assert_eq!(e[0].kind, EntryKind::Dir);
        assert_eq!(e[0].modified, "Jan 2 2024");
        assert_eq!(e[1].link_target.as_deref(), Some("target"));
        assert!(e[1].is_dir_like());
        assert_eq!(e[3].size, 220);
        assert_eq!(e[4].kind, EntryKind::Other);
        assert_eq!(e[4].size, 0);
    }

    #[test]
    fn pwd_then_listing() {
        let (pwd, e) = parse_pwd_ls("/home/me\ntotal 0\n-rw-r--r-- 1 0 0 1 Oct 13 12:00 a\n");
        assert_eq!(pwd.as_deref(), Some("/home/me"));
        assert_eq!(e.len(), 1);
        assert_eq!(parse_pwd_ls("").0, None);
    }

    #[test]
    fn truncated_lines_do_not_panic() {
        for l in ["-", "-rw-r--r--", "-rw-r--r-- 1", "-rw-r--r-- 1 0 0 5 Oct", "drwx------ 1 0 0 5 Oct 1 12:00", "é"] {
            assert!(parse_ls(l).is_empty(), "{l}");
        }
    }
}
