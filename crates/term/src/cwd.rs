//! The working directory of running processes (a shell's, after the user `cd`s), for shells that
//! do not report it with OSC 7 (macOS zsh only does that inside Terminal.app).
//!
//! Linux reads `/proc/<pid>/cwd`; macOS asks `lsof` (one call for all the processes). Blocking:
//! call it off the UI thread.

use std::path::PathBuf;

/// The current directory of each of `pids` that could be read (a process that exited, or one the
/// system does not tell us about, is left out).
pub fn process_cwds(pids: &[u32]) -> Vec<(u32, PathBuf)> {
    if pids.is_empty() {
        return Vec::new();
    }
    if cfg!(target_os = "linux") {
        return pids
            .iter()
            .filter_map(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| (*pid, p)))
            .collect();
    }
    if !cfg!(unix) {
        return Vec::new();
    }
    let list = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let lsof = ["/usr/sbin/lsof", "/usr/bin/lsof"].into_iter().find(|p| std::path::Path::new(p).is_file());
    let Some(lsof) = lsof else { return Vec::new() };
    // `lsof` exits 1 when one of the processes is gone but still prints the others.
    match std::process::Command::new(lsof)
        .args(["-a", "-d", "cwd", "-Fn", "-w", "-p", &list])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(out) => parse_lsof(&String::from_utf8_lossy(&out.stdout), pids),
        Err(e) => {
            log::debug!("lsof: {e}");
            Vec::new()
        }
    }
}

/// `lsof -F n` output: `p<pid>` starts a process, `n<path>` is the cwd's name.
fn parse_lsof(text: &str, pids: &[u32]) -> Vec<(u32, PathBuf)> {
    let mut out = Vec::new();
    let mut pid = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.trim().parse::<u32>().ok().filter(|p| pids.contains(p));
        } else if let Some(name) = line.strip_prefix('n')
            && let Some(p) = pid.take()
            && name.starts_with('/')
        {
            out.push((p, PathBuf::from(name)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_fields() {
        let text = "p12\nfcwd\nn/Users/a b/src\np99\nfcwd\nn/tmp\np7\nfcwd\nnrelative\n";
        assert_eq!(parse_lsof(text, &[12, 7]), vec![(12, PathBuf::from("/Users/a b/src"))]);
        assert!(parse_lsof("", &[1]).is_empty());
        assert!(parse_lsof("pxyz\nn/a\nn/b\n", &[1]).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn finds_a_child_process_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("/bin/sleep").arg("30").current_dir(dir.path()).spawn().unwrap();
        let found = process_cwds(&[child.id(), 999_999]);
        let _ = child.kill();
        let _ = child.wait();
        let want = dir.path().canonicalize().unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, child.id());
        assert_eq!(found[0].1.canonicalize().unwrap(), want);
        assert!(process_cwds(&[]).is_empty());
    }
}
