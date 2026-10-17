//! `release.sh`: bump, commit, tag and push to a local bare "origin" in a temp directory (a fake
//! `xtask version`, no cargo, no network), and every refusal leaves the tree as it was.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FAKE_XTASK: &str = r#"#!/bin/bash
set -e
[ "$1" = version ] || exit 2
cur=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
[ -z "$2" ] && { echo "$cur"; exit 0; }
IFS=. read -r ma mi pa <<<"$cur"
case "$2" in
  patch) n="$ma.$mi.$((pa+1))" ;;
  minor) n="$ma.$((mi+1)).0" ;;
  major) n="$((ma+1)).0.0" ;;
  *) n="$2" ;;
esac
if [ "$3" != --dry-run ]; then
  for f in Cargo.toml Cargo.lock; do sed "s/^version = \"$cur\"/version = \"$n\"/" "$f" > "$f.new"; mv "$f.new" "$f"; done
fi
echo "$n"
"#;

struct Repo {
    _dir: tempfile::TempDir,
    work: PathBuf,
    origin: PathBuf,
    xtask: PathBuf,
}

fn git_cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid");
    c
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn setup() -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin.git");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(dir.path(), &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
    git(&work, &["init", "-q", "-b", "main"]);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../release.sh");
    std::fs::copy(&script, work.join("release.sh")).unwrap();
    std::fs::write(work.join("Cargo.toml"), "[workspace.package]\nversion = \"0.1.0\"\nedition = \"2024\"\n").unwrap();
    std::fs::write(work.join("Cargo.lock"), "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n")
        .unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    git(&work, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&work, &["push", "-q", "origin", "main"]);
    let xtask = dir.path().join("fake-xtask");
    std::fs::write(&xtask, FAKE_XTASK).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&xtask, std::fs::Permissions::from_mode(0o755)).unwrap();
    Repo { _dir: dir, work, origin, xtask }
}

/// Run `release.sh` (with the same isolated git environment) in the work tree.
fn release(r: &Repo, args: &[&str], lock_ok: bool) -> Output {
    let env = git_cmd(&r.work);
    let mut script = Command::new("bash");
    for (k, v) in env.get_envs() {
        if let Some(v) = v {
            script.env(k, v);
        }
    }
    script
        .current_dir(&r.work)
        .arg(r.work.join("release.sh"))
        .args(args)
        .env("CODECRAFT_RELEASE_XTASK", &r.xtask)
        .env("CODECRAFT_RELEASE_LOCK_CHECK", if lock_ok { "true" } else { "false" })
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn origin_has_tag(r: &Repo, tag: &str) -> bool {
    git_cmd(&r.origin)
        .args(["rev-parse", "-q", "--verify", &format!("refs/tags/{tag}")])
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn bumps_commits_tags_and_pushes() {
    let r = setup();
    let out = release(&r, &["minor", "--yes"], true);
    assert!(out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(t.contains("0.1.0 → 0.2.0"), "{t}");
    assert!(t.contains("gh workflow run release.yml -f tag=v0.2.0"), "{t}");
    // On origin: the release commit on main and the annotated tag pointing at it.
    assert_eq!(git(&r.origin, &["log", "-1", "--format=%s", "main"]), "Release v0.2.0");
    assert!(origin_has_tag(&r, "v0.2.0"));
    assert_eq!(git(&r.origin, &["cat-file", "-t", "v0.2.0"]), "tag", "annotated");
    assert_eq!(git(&r.origin, &["rev-parse", "v0.2.0^{commit}"]), git(&r.origin, &["rev-parse", "main"]));
    let toml = git(&r.origin, &["show", "main:Cargo.toml"]);
    let lock = git(&r.origin, &["show", "main:Cargo.lock"]);
    assert!(toml.contains("version = \"0.2.0\"") && lock.contains("version = \"0.2.0\""), "{toml}\n{lock}");
    assert!(lock.starts_with("version = 4"), "the lock format line is untouched");
    assert_eq!(git(&r.work, &["status", "--porcelain"]), "");

    // Patch and an exact pre-release, kept local.
    let out = release(&r, &["patch", "--yes"], true);
    assert!(out.status.success(), "{}", text(&out));
    assert!(origin_has_tag(&r, "v0.2.1"));
    let out = release(&r, &["v0.3.0-beta.1", "--yes", "--no-push"], true);
    assert!(out.status.success(), "{}", text(&out));
    assert!(git(&r.work, &["tag"]).contains("v0.3.0-beta.1"));
    assert!(!origin_has_tag(&r, "v0.3.0-beta.1"), "--no-push pushes nothing");
    assert!(text(&out).contains("git push --atomic origin main v0.3.0-beta.1"));
}

#[test]
fn refusals_change_nothing() {
    let r = setup();
    let head = git(&r.work, &["rev-parse", "HEAD"]);
    let unchanged = |r: &Repo| {
        assert_eq!(git(&r.work, &["rev-parse", "HEAD"]), head);
        assert!(std::fs::read_to_string(r.work.join("Cargo.toml")).unwrap().contains("version = \"0.1.0\""));
    };

    // No confirmation (stdin closed) → cancelled.
    let out = release(&r, &["minor"], true);
    assert!(!out.status.success());
    unchanged(&r);

    // Dry run.
    let out = release(&r, &["major", "--dry-run"], true);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("0.1.0 → 1.0.0"));
    unchanged(&r);

    // Bad argument.
    let out = release(&r, &["sideways"], true);
    assert!(!out.status.success());

    // The lock check fails → the bump is undone, nothing committed or tagged.
    let out = release(&r, &["minor", "--yes"], false);
    assert!(!out.status.success(), "{}", text(&out));
    unchanged(&r);
    assert_eq!(git(&r.work, &["status", "--porcelain"]), "");
    assert_eq!(git(&r.work, &["tag"]), "");

    // The tag exists already.
    git(&r.work, &["tag", "v0.2.0"]);
    let out = release(&r, &["minor", "--yes"], true);
    assert!(!out.status.success());
    assert!(text(&out).contains("tag v0.2.0 already exists"), "{}", text(&out));
    unchanged(&r);
    git(&r.work, &["tag", "-d", "v0.2.0"]);

    // A dirty tree.
    std::fs::write(r.work.join("notes.txt"), "wip").unwrap();
    let out = release(&r, &["minor", "--yes"], true);
    assert!(!out.status.success());
    assert!(text(&out).contains("working tree has changes"), "{}", text(&out));
    unchanged(&r);
    std::fs::remove_file(r.work.join("notes.txt")).unwrap();

    // Not on main.
    git(&r.work, &["checkout", "-q", "-b", "feature"]);
    let out = release(&r, &["minor", "--yes"], true);
    assert!(!out.status.success());
    assert!(text(&out).contains("switch to main"), "{}", text(&out));
    git(&r.work, &["checkout", "-q", "main"]);

    // Behind origin/main.
    let other = r.work.parent().unwrap().join("other");
    git(r.work.parent().unwrap(), &["clone", "-q", r.origin.to_str().unwrap(), other.to_str().unwrap()]);
    std::fs::write(other.join("x.txt"), "x").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-q", "-m", "elsewhere"]);
    git(&other, &["push", "-q", "origin", "main"]);
    let out = release(&r, &["minor", "--yes"], true);
    assert!(!out.status.success());
    assert!(text(&out).contains("behind origin/main"), "{}", text(&out));
    unchanged(&r);
    assert!(!origin_has_tag(&r, "v0.2.0"));
}

/// The script's default lock check must work offline on this workspace without downloading the
/// crates of other platforms (`cargo metadata --offline` failed on `wasi` this way).
#[test]
fn default_lock_check_works_offline() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let script = std::fs::read_to_string(root.join("release.sh")).unwrap();
    let line = script.lines().find(|l| l.starts_with("LOCK_CHECK=")).unwrap();
    let cmd = line.split_once(":-").unwrap().1.trim_end_matches('}');
    let words: Vec<&str> = cmd.split_whitespace().collect();
    assert_eq!(words.first(), Some(&"cargo"), "{cmd}");
    assert!(words.contains(&"--locked") && words.contains(&"--offline"), "{cmd}");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo).args(&words[1..]).current_dir(root).output().unwrap();
    assert!(out.status.success(), "{cmd}: {}", String::from_utf8_lossy(&out.stderr));
}
