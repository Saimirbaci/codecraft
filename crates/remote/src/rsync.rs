//! `rsync --itemize-changes` output: one `YXcstpoguax path` line per changed item, plus
//! `*deleting   path` lines. Works with rsync 2.6 (9 flag characters), 3.x (11) and openrsync.

use serde::{Deserialize, Serialize};

/// What happens to an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Change {
    /// Content is transferred (`<` sent / `>` received).
    Transfer,
    /// Created without a transfer (`c`: directories, symlinks).
    Create,
    /// Deleted on the receiver (`*deleting`).
    Delete,
    /// Only attributes change (`.`), or a hard link (`h`).
    Attributes,
}

/// The kind of an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One itemized line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemizedChange {
    /// Relative to the transfer root, `/`-separated, without a trailing `/`.
    pub path: String,
    pub kind: ItemKind,
    pub change: Change,
    /// The item does not exist on the receiver yet.
    pub new: bool,
}

/// Parse itemized output. Lines that are not itemized (file list headers, the summary) are skipped.
pub fn parse_itemize(output: &str) -> Vec<ItemizedChange> {
    output.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<ItemizedChange> {
    let line = line.trim_end_matches('\r');
    if let Some(rest) = line.strip_prefix("*deleting") {
        let raw = rest.trim_start();
        if raw.is_empty() {
            return None;
        }
        let kind = if raw.ends_with('/') { ItemKind::Dir } else { ItemKind::File };
        return Some(ItemizedChange { path: clean(raw), kind, change: Change::Delete, new: false });
    }
    let (flags, path) = line.split_once(' ')?;
    let fl: Vec<char> = flags.chars().collect();
    if !(9..=11).contains(&fl.len()) {
        return None;
    }
    let change = match fl.first()? {
        '<' | '>' => Change::Transfer,
        'c' => Change::Create,
        '.' | 'h' => Change::Attributes,
        _ => return None,
    };
    let kind = match fl.get(1)? {
        'f' => ItemKind::File,
        'd' => ItemKind::Dir,
        'L' => ItemKind::Symlink,
        'D' | 'S' => ItemKind::Other,
        _ => return None,
    };
    if !fl.iter().skip(2).all(|c| c.is_ascii_alphabetic() || matches!(c, '.' | '+' | ' ' | '?')) {
        return None;
    }
    let new = fl.iter().skip(2).all(|c| *c == '+');
    let mut path = path;
    if kind == ItemKind::Symlink
        && let Some((p, _target)) = path.split_once(" -> ")
    {
        path = p;
    }
    let path = clean(path);
    if path.is_empty() || path == "." {
        return None;
    }
    Some(ItemizedChange { path, kind, change, new })
}

fn clean(p: &str) -> String {
    let p = p.trim_end_matches('/');
    p.strip_prefix("./").unwrap_or(p).to_string()
}

/// The changes worth showing in a sync preview (no attribute-only updates).
pub fn significant(changes: &[ItemizedChange]) -> Vec<ItemizedChange> {
    changes.iter().filter(|c| c.change != Change::Attributes).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = "sending incremental file list\n\
.d..t...... ./\n\
>f+++++++++ src/new file.rs\n\
<f.st...... src/main.rs\n\
cd+++++++++ assets/\n\
cL+++++++++ link -> target/x\n\
*deleting   old/gone.txt\n\
*deleting   olddir/\n\
.f...p..... perms.sh\n\
>f.st.... legacy.rs\n\
\n\
sent 1,234 bytes  received 56 bytes  2,580.00 bytes/sec\n\
total size is 99  speedup is 0.08 (DRY RUN)\n";

    #[test]
    fn parses_every_kind_of_line() {
        let c = parse_itemize(OUT);
        let paths: Vec<&str> = c.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["src/new file.rs", "src/main.rs", "assets", "link", "old/gone.txt", "olddir", "perms.sh", "legacy.rs"]
        );
        assert_eq!(
            c[0],
            ItemizedChange {
                path: "src/new file.rs".into(),
                kind: ItemKind::File,
                change: Change::Transfer,
                new: true
            }
        );
        assert!(!c[1].new);
        assert_eq!(c[2].kind, ItemKind::Dir);
        assert_eq!(c[2].change, Change::Create);
        assert_eq!(c[3].kind, ItemKind::Symlink);
        assert_eq!(c[4].change, Change::Delete);
        assert_eq!(c[5].kind, ItemKind::Dir);
        assert_eq!(c[6].change, Change::Attributes);
        assert_eq!(c[7].change, Change::Transfer);
        assert_eq!(significant(&c).len(), 7);
    }

    #[test]
    fn garbage_is_ignored() {
        assert!(parse_itemize("x\n>\n>f\n>q+++++++++ a\n*deleting\n\u{0}\u{1} \n>f+++++++++ \n").is_empty());
    }
}
