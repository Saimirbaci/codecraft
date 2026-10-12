//! Process-tree termination: the terminal's child, every descendant and their process groups.
//!
//! An interactive shell puts each job in its own process group, so killing the shell's group
//! alone would leave `vim`, `cargo watch` or a background `sleep` running. Before anything dies
//! we snapshot the descendants (`ps`), then send SIGHUP (and SIGCONT, so stopped jobs see it) to
//! every group and process, wait a short grace period, and SIGKILL whatever is left.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, getpgrp, kill_process, kill_process_group, test_kill_process};

/// `(pid, ppid, pgid)` of every process, from `ps`.
fn process_table() -> Vec<(i32, i32, i32)> {
    let ps = if std::path::Path::new("/bin/ps").exists() { "/bin/ps" } else { "ps" };
    let Ok(out) = std::process::Command::new(ps)
        .args(["-A", "-o", "pid=", "-o", "ppid=", "-o", "pgid="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace().map(|v| v.parse::<i32>().ok());
            Some((it.next()??, it.next()??, it.next()??))
        })
        .collect()
}

/// `root` and its descendants as `(pid, pgid)`.
pub(crate) fn descendants(root: i32) -> Vec<(i32, i32)> {
    let table = process_table();
    let mut out = Vec::new();
    let mut frontier = vec![root];
    let mut seen = BTreeSet::new();
    while let Some(p) = frontier.pop() {
        if !seen.insert(p) {
            continue;
        }
        if let Some((_, _, pgid)) = table.iter().find(|(pid, _, _)| *pid == p) {
            out.push((p, *pgid));
        } else if p == root {
            out.push((p, p));
        }
        frontier.extend(table.iter().filter(|(_, ppid, _)| *ppid == p).map(|(pid, _, _)| *pid));
    }
    out
}

fn alive(pid: i32) -> bool {
    Pid::from_raw(pid).is_some_and(|p| test_kill_process(p).is_ok())
}

fn signal_all(pids: &[i32], groups: &BTreeSet<i32>, sig: Signal) {
    for g in groups {
        if let Some(p) = Pid::from_raw(*g) {
            let _ = kill_process_group(p, sig);
        }
    }
    for pid in pids {
        if let Some(p) = Pid::from_raw(*pid) {
            let _ = kill_process(p, sig);
        }
    }
}

/// Terminate `root` (a session leader we spawned and have not reaped), its descendants and
/// their process groups. Never signals our own process group. Blocks for at most `grace` plus
/// the time `ps` takes. `root_exited` tells when `root` is only a zombie (no need to wait for it).
pub(crate) fn kill_tree(root: u32, grace: Duration, root_exited: &AtomicBool) {
    let Some(root) = i32::try_from(root).ok().filter(|p| *p > 1) else { return };
    let own_group = getpgrp().as_raw_nonzero().get();
    let own_pid = rustix::process::getpid().as_raw_nonzero().get();
    let tree = descendants(root);
    let pids: Vec<i32> = tree.iter().map(|(p, _)| *p).filter(|p| *p > 1 && *p != own_pid).collect();
    let mut groups: BTreeSet<i32> = tree.iter().map(|(_, g)| *g).collect();
    groups.insert(root);
    groups.retain(|g| *g > 1 && *g != own_group);
    signal_all(&pids, &groups, Signal::HUP);
    signal_all(&pids, &groups, Signal::CONT);
    let deadline = Instant::now() + grace;
    let pending = |p: i32| if p == root { !root_exited.load(Ordering::SeqCst) } else { alive(p) };
    while Instant::now() < deadline && pids.iter().any(|p| pending(*p)) {
        std::thread::sleep(Duration::from_millis(15));
    }
    signal_all(&pids, &groups, Signal::KILL);
}
