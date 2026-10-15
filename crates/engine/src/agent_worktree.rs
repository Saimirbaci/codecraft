//! Review jobs' tool calls in their worktree (M15 track AI).
//!
//! A Review job works in a git worktree; the open project must not change until its user accepts
//! the changes. Its engine tool calls (files, runs, tests, inspections, debugging, profiling,
//! databases) therefore run in a second, headless [`Session`] opened on the job's directory in
//! the worktree, with the same machinery as the IDE's own session. The IDE's session still
//! decides what the job may do (the access policy and its approval cards); the worktree session
//! only executes. It is pumped from the IDE's pump and closed once the job stops running.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::Session;
use crate::agent_access::{AgentAccess, Capability};
use crate::agents::JobId;

/// The session serving one Review job's calls.
pub(crate) struct WorktreeSession {
    pub job: JobId,
    /// The job's directory inside the worktree (the session's project root).
    pub root: PathBuf,
    pub session: Box<Session>,
}

/// Open a headless session on `root` for `job`'s calls, configured like `parent` (language
/// servers, an isolated home in tests, the password store).
pub(crate) fn open(parent: &Session, job: JobId, root: &Path) -> Result<WorktreeSession, String> {
    if root.as_os_str().is_empty() || !root.is_dir() {
        return Err(format!(
            "the job's worktree is gone{}; a Review job's tools act only on its worktree",
            if root.as_os_str().is_empty() { String::new() } else { format!(" ({})", root.display()) }
        ));
    }
    let mut inner = Box::new(Session::new());
    inner.lsp_enabled = parent.lsp_enabled;
    if let Some(home) = &parent.isolated_home {
        inner.isolate_home(home);
    }
    inner.db.set_secret_store(parent.db.secret_store());
    // No Claude Code lock file for the worktree; nobody answers prompts here (the IDE's session
    // already applied the policy and asked the user).
    inner.ide.set_enabled(false);
    inner.agent_access = AgentAccess::headless(&Capability::ALL);
    ignore_own_state(root);
    inner
        .execute("file.open", &serde_json::json!({ "path": root }))
        .map_err(|e| format!("cannot open the worktree {}: {e}", root.display()))?;
    Ok(WorktreeSession { job, root: root.to_path_buf(), session: inner })
}

/// CodeCraft keeps per-project state in `.codecraft/` (breakpoints, test history, local
/// history). In a worktree that the project does not track, ignore it, so the job's review lists
/// only the agent's changes.
fn ignore_own_state(root: &Path) {
    let dir = root.join(".codecraft");
    if dir.exists() {
        return;
    }
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join(".gitignore"), "# CodeCraft state of an agent's worktree\n*\n");
    }
}

/// Close a worktree session without blocking the caller (language servers take a moment to
/// shut down).
pub(crate) fn close(mut ws: WorktreeSession) {
    let spawned = std::thread::Builder::new().name("codecraft-worktree-session-close".into()).spawn(move || {
        ws.session.shutdown();
    });
    if let Err(e) = spawned {
        log::warn!("cannot close a worktree session in the background: {e}");
    }
}

/// `value`'s strings that are paths inside `project` (the open project), moved to the same place
/// under `root` (the job's worktree): agents sometimes keep absolute paths of the project they
/// were started from. Only the top level and `params` (of `codecraft_execute`) are looked at.
pub(crate) fn remap_paths(value: &Value, project: &Path, root: &Path) -> Value {
    let map_str = |s: &str| -> Option<String> {
        let p = Path::new(s);
        if !p.is_absolute() || p.starts_with(root) {
            return None;
        }
        let rel = p.strip_prefix(project).ok()?;
        Some(root.join(rel).to_string_lossy().into_owned())
    };
    let map_obj = |o: &serde_json::Map<String, Value>| -> serde_json::Map<String, Value> {
        o.iter()
            .map(|(k, v)| {
                let v = match v {
                    Value::String(s) => map_str(s).map(Value::String).unwrap_or_else(|| v.clone()),
                    _ => v.clone(),
                };
                (k.clone(), v)
            })
            .collect()
    };
    let Value::Object(o) = value else { return value.clone() };
    let mut out = map_obj(o);
    if let Some(Value::Object(params)) = out.get("params") {
        let p = map_obj(params);
        out.insert("params".into(), Value::Object(p));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_paths_move_into_the_worktree() {
        let project = Path::new("/work/app");
        let root = Path::new("/wt/12/app");
        let v = remap_paths(
            &json!({ "path": "/work/app/src/main.rs", "file": "src/x.rs", "line": 3, "other": "/etc/hosts",
                     "params": { "path": "/work/app/a.py", "n": 1 }, "program": "/wt/12/app/bin" }),
            project,
            root,
        );
        assert_eq!(v["path"], "/wt/12/app/src/main.rs");
        assert_eq!(v["file"], "src/x.rs", "relative paths are already relative to the worktree");
        assert_eq!(v["other"], "/etc/hosts");
        assert_eq!(v["params"]["path"], "/wt/12/app/a.py");
        assert_eq!(v["program"], "/wt/12/app/bin");
        assert_eq!(v["line"], 3);
        assert_eq!(remap_paths(&json!([1]), project, root), json!([1]));
        // A worktree inside the project (unusual, but possible) is not remapped into itself.
        let inner = Path::new("/work/app/.wt/1");
        assert_eq!(remap_paths(&json!({ "path": "/work/app/.wt/1/a" }), project, inner)["path"], "/work/app/.wt/1/a");
    }

    #[test]
    fn a_missing_worktree_is_an_error() {
        let parent = Session::new();
        let Err(e) = open(&parent, JobId(1), Path::new("")) else { panic!("opened nothing") };
        assert!(e.contains("worktree is gone"), "{e}");
        let tmp = tempfile::tempdir().unwrap();
        let Err(e) = open(&parent, JobId(1), &tmp.path().join("gone")) else { panic!("opened a missing dir") };
        assert!(e.contains("gone"), "{e}");
    }

    #[test]
    fn own_state_is_ignored_in_an_untracked_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        ignore_own_state(tmp.path());
        assert!(std::fs::read_to_string(tmp.path().join(".codecraft/.gitignore")).unwrap().contains('*'));
        // A project that tracks `.codecraft/` keeps it as it is.
        let tracked = tmp.path().join("tracked");
        std::fs::create_dir_all(tracked.join(".codecraft")).unwrap();
        ignore_own_state(&tracked);
        assert!(!tracked.join(".codecraft/.gitignore").exists());
    }
}
