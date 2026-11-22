//! Terminal tabs on a remote project's host (SSH): the same per-tab command history and Claude
//! Code session tracking local tabs have ([`crate::terminal_shell`]), kept on the host.
//!
//! Everything travels in the one `ssh` command that opens the tab: [`launch_script`] writes the
//! same startup files and `claude` shim under `~/.codecraft/terminals/` on the host (private,
//! rewritten on every connect, nothing else is touched), points the tab's shell at its own
//! history file there and starts it. The Claude Code hook writes the tab's session record on the
//! host; [`POLL_SCRIPT`] reads all records back (through the project's shared connection) so the
//! session snapshot has the last known session of each tab without blocking on a connection,
//! and [`parse_poll`] reads its answer.
//!
//! Nothing here runs unless the session keeps terminal files (like local tabs: tests and the
//! CLI start the plain login shell).

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use codecraft_remote::quote::{quote_path, sh_quote};

use crate::terminal_shell::{
    BASHRC, CLAUDE_HOOK, CLAUDE_SHIM, ClaudeState, HEADER, ZPROFILE, ZSHENV, ZSHRC, parse_claude_state, valid_id,
};

/// Where the host keeps the files, relative to `$HOME`.
const REMOTE_DIR: &str = ".codecraft/terminals";
/// The answers are small; a runaway host cannot fill memory.
const MAX_POLL_BYTES: usize = 1024 * 1024;
const MARKER: &str = "@@codecraft-claude@@";
/// A lookup or cleanup that takes longer is abandoned (the next poll tries again).
const SSH_TIMEOUT: Duration = Duration::from_secs(20);

/// What the host knows about the Claude Code session of one tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteClaude {
    pub state: ClaudeState,
    /// The `claude` process still runs there (a session that ended is not resumed).
    pub alive: bool,
}

/// The `sh` script an interactive tab runs on the host: `cd` into `directory`, install the
/// integration, then `exec` the login shell with it. When the files cannot be written (a
/// read-only or odd home) the tab is the plain login shell, as before.
pub(crate) fn launch_script(key: &str, directory: &str) -> String {
    let plain = "exec \"${SHELL:-/bin/sh}\" -l\n";
    let mut s = format!("cd {} || exit 1\n", quote_path(directory));
    if !valid_id(key) {
        s.push_str(plain);
        return s;
    }
    s.push_str(&format!("d=\"$HOME/{REMOTE_DIR}\"\nk={key}\n"));
    // The settings file holds the hook's path inside JSON; keep to plain paths.
    s.push_str(&format!("case $d in *[!A-Za-z0-9/_.-]*) {plain};; esac\n"));
    s.push_str("w() { printf '%s\\n' \"$2\" > \"$1\"; }\n");
    s.push_str("u=$(umask); umask 077\n");
    let files: [(&str, &str); 6] = [
        ("zsh/.zshenv", ZSHENV),
        ("zsh/.zprofile", ZPROFILE),
        ("zsh/.zshrc", ZSHRC),
        ("bashrc", BASHRC),
        ("bin/claude", CLAUDE_SHIM),
        ("claude-session-hook.sh", CLAUDE_HOOK),
    ];
    s.push_str("if mkdir -p \"$d/zsh\" \"$d/bin\" 2>/dev/null");
    for (name, text) in files {
        // The local copies of the executables start with a shebang.
        let text = if name.starts_with("bin/") || name.ends_with(".sh") {
            format!("#!/bin/sh\n{HEADER}{text}")
        } else {
            format!("{HEADER}{text}")
        };
        s.push_str(&format!(" \\\n  && w \"$d/{name}\" {}", sh_quote(&text)));
    }
    s.push_str(" \\\n  && chmod 755 \"$d/bin/claude\" \"$d/claude-session-hook.sh\"");
    s.push_str(
        " \\\n  && printf '{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"/bin/sh %s/claude-session-hook.sh\"}]}]}}\\n' \"$d\" > \"$d/claude-settings.json\"",
    );
    s.push_str("; then :; else umask \"$u\"; ");
    s.push_str(plain.trim_end());
    s.push_str("; fi\numask \"$u\"\n");
    s.push_str("CODECRAFT_HISTFILE=\"$d/$k.history\"\n");
    s.push_str("CODECRAFT_CLAUDE_STATE=\"$d/$k.claude.json\"\n");
    s.push_str("CODECRAFT_CLAUDE_SETTINGS=\"$d/claude-settings.json\"\n");
    s.push_str("CODECRAFT_TERMINAL_BIN=\"$d/bin\"\n");
    s.push_str("export CODECRAFT_HISTFILE CODECRAFT_CLAUDE_STATE CODECRAFT_CLAUDE_SETTINGS CODECRAFT_TERMINAL_BIN\n");
    s.push_str("PATH=\"$d/bin:$PATH\"; export PATH\n");
    s.push_str("sh=${SHELL:-/bin/sh}\n");
    let session: String = key.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    s.push_str("case ${sh##*/} in\n");
    s.push_str(
        "  zsh|-zsh)\n    if [ -n \"${ZDOTDIR:-}\" ]; then CODECRAFT_USER_ZDOTDIR=$ZDOTDIR; CODECRAFT_ZDOTDIR_SET=1; export CODECRAFT_USER_ZDOTDIR CODECRAFT_ZDOTDIR_SET; fi\n    ZDOTDIR=\"$d/zsh\"; export ZDOTDIR\n    exec \"$sh\" -l ;;\n",
    );
    s.push_str("  bash|-bash) exec \"$sh\" --rcfile \"$d/bashrc\" -i ;;\n");
    s.push_str(&format!(
        "  fish|-fish) exec \"$sh\" -l -i -C 'set -g fish_history codecraft_{session}; set -gx PATH $CODECRAFT_TERMINAL_BIN $PATH' ;;\n"
    ));
    s.push_str("  *) exec \"$sh\" -l ;;\nesac\n");
    s
}

/// Prints every tab's session record: a marker line (`key`, and 1 when the `claude` process
/// still runs) followed by the record.
pub(crate) fn poll_script() -> String {
    format!(
        "d=\"$HOME/{REMOTE_DIR}\"\nfor f in \"$d\"/*.claude.json; do\n  [ -f \"$f\" ] || continue\n  k=${{f##*/}}; k=${{k%.claude.json}}\n  pid=$(sed -n 's/^{{\"pid\":\"\\([0-9][0-9]*\\)\".*/\\1/p' \"$f\" | head -n 1)\n  alive=0; [ -n \"$pid\" ] && kill -0 \"$pid\" 2>/dev/null && alive=1\n  printf '%s %s %s\\n' '{MARKER}' \"$k\" \"$alive\"\n  head -c 65536 \"$f\"\ndone\n"
    )
}

/// Removes a closed tab's files on the host.
pub(crate) fn forget_script(key: &str) -> Option<String> {
    valid_id(key).then(|| {
        format!(
            "d=\"$HOME/{REMOTE_DIR}\"; rm -f \"$d/{key}.history\" \"$d/{key}.claude.json\" \"$d/{key}.claude.json\".*\n"
        )
    })
}

/// The records in a [`poll_script`] answer; anything unreadable is skipped.
pub(crate) fn parse_poll(output: &str) -> Vec<(String, RemoteClaude)> {
    let mut out = Vec::new();
    let mut current: Option<(String, bool, String)> = None;
    let mut finish = |current: Option<(String, bool, String)>| {
        if let Some((key, alive, text)) = current
            && valid_id(&key)
            && let Some(state) = parse_claude_state(&text)
        {
            out.push((key, RemoteClaude { state, alive }));
        }
    };
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix(MARKER) {
            finish(current.take());
            let mut words = rest.split_whitespace();
            if let (Some(key), Some(alive)) = (words.next(), words.next()) {
                current = Some((key.to_string(), alive == "1", String::new()));
            }
        } else if let Some((_, _, text)) = current.as_mut() {
            text.push_str(line);
            text.push('\n');
        }
    }
    finish(current.take());
    out
}

/// Run `program args…` (an `ssh` with its remote command) and return what it printed; `None`
/// when it cannot start, fails, or takes longer than [`SSH_TIMEOUT`]. Runs on the caller's
/// (background) thread.
pub(crate) fn run_ssh(program: &std::path::Path, args: &[String]) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| log::debug!("terminal ssh: cannot run {}: {e}", program.display()))
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stdout).take(MAX_POLL_BYTES as u64).read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + SSH_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let bytes = reader.join().unwrap_or_default();
    status.filter(|s| s.success()).map(|_| String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_answers_are_read_defensively() {
        let out = format!(
            "{MARKER} tab-1 1\n{{\"pid\":\"42\",\"hook\":{{\"session_id\":\"s-1\",\"cwd\":\"/srv/app/api\",\"transcript_path\":\"/r/s-1.jsonl\"}}\n}}\n\
             {MARKER} tab-2 0\n{{\"pid\":\"\",\"hook\":{{\"session_id\":\"s-2\"}}}}\n\
             {MARKER} ../bad 1\n{{\"hook\":{{\"session_id\":\"s-3\"}}}}\n\
             {MARKER} tab-4 1\nnot json\n\
             {MARKER} tab-5 1\n{{\"hook\":{{\"session_id\":\"$(rm -rf)\"}}}}\n\
             {MARKER}\nstray\n"
        );
        let found = parse_poll(&out);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].0, "tab-1");
        assert!(found[0].1.alive);
        assert_eq!(found[0].1.state.session, "s-1");
        assert_eq!(found[0].1.state.cwd.as_deref(), Some("/srv/app/api"));
        assert_eq!((found[1].0.as_str(), found[1].1.alive), ("tab-2", false));
        assert!(parse_poll("").is_empty() && parse_poll("garbage\n").is_empty());
    }

    #[test]
    fn forget_only_for_plain_keys() {
        assert!(forget_script("2f6c-1d").is_some_and(|s| s.contains("2f6c-1d.history")));
        for bad in ["", "../x", "a b", "x;rm -rf ~"] {
            assert!(forget_script(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_bad_key_gets_the_plain_login_shell() {
        let s = launch_script("a b", "~/app");
        assert!(s.contains("exec \"${SHELL:-/bin/sh}\" -l") && !s.contains("CODECRAFT_HISTFILE"), "{s}");
    }

    /// Run the launch script like the host would: `sh -c`, with `$HOME` and `$SHELL` of its own,
    /// the shell reading `input` (so it is the "tab").
    #[cfg(unix)]
    fn run_tab(home: &std::path::Path, shell: &str, key: &str, input: &str) -> String {
        use std::io::Write;
        let script = launch_script(key, &home.display().to_string());
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("HOME", home)
            .env("SHELL", shell)
            .env("PATH", "/usr/bin:/bin")
            .env("BASH_SILENCE_DEPRECATION_WARNING", "1")
            .env_remove("ZDOTDIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    }

    #[cfg(unix)]
    #[test]
    fn a_remote_tab_keeps_its_own_history_and_records_claude_sessions() {
        for shell in ["/bin/bash", "/bin/zsh"] {
            if !std::path::Path::new(shell).is_file() {
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().canonicalize().unwrap();
            std::fs::write(home.join(".bash_history"), "echo from-global-history\n").unwrap();
            let state = home.join(".codecraft/terminals/tab-1.claude.json");
            // What Claude Code's hook does in the tab: record the session, with the "claude pid"
            // being this shell (which is still alive when the poll runs).
            let input = format!(
                "echo typed-in-tab-one\nprintf '{{\"session_id\":\"sess-1\",\"cwd\":\"/srv/app\"}}' | CODECRAFT_CLAUDE_PID=$$ /bin/sh \"$CODECRAFT_TERMINAL_BIN/../claude-session-hook.sh\"\nprintf 'bin=%s\\n' \"$CODECRAFT_TERMINAL_BIN\" > '{}'\nsh -c '{}' > '{}'\nexit\n",
                home.join("seen").display(),
                poll_script().replace('\'', "'\\''"),
                home.join("poll").display()
            );
            if shell.ends_with("zsh") {
                // Without a terminal zsh is not interactive and keeps no history: check what the
                // tab's shell is given instead (the real thing runs on the host's pty).
                let out = run_tab(&home, shell, "tab-1", "echo hist=$CODECRAFT_HISTFILE zdot=$ZDOTDIR\nexit\n");
                let term = home.join(".codecraft/terminals");
                assert!(out.contains(&format!("hist={}/tab-1.history", term.display())), "{out}");
                assert!(out.contains(&format!("zdot={}/zsh", term.display())), "{out}");
                assert!(term.join("zsh/.zshrc").is_file());
                continue;
            }
            let out = run_tab(&home, shell, "tab-1", &input);
            let term = home.join(".codecraft/terminals");
            assert!(
                term.join("bin/claude").is_file() && term.join("claude-session-hook.sh").is_file(),
                "{shell}: {out}"
            );
            let settings: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(term.join("claude-settings.json")).unwrap()).unwrap();
            let hook = settings.pointer("/hooks/SessionStart/0/hooks/0/command").and_then(|c| c.as_str()).unwrap();
            assert!(hook.starts_with("/bin/sh ") && hook.ends_with("claude-session-hook.sh"), "{hook}");
            let history = std::fs::read_to_string(term.join("tab-1.history")).unwrap_or_default();
            assert!(history.contains("typed-in-tab-one"), "{shell}: the tab's own history: {history:?}\n{out}");
            assert!(!history.contains("from-global-history"), "{shell}: {history:?}");
            assert!(std::fs::read_to_string(home.join("seen")).unwrap().contains(".codecraft/terminals/bin"));
            assert!(state.is_file(), "{shell}: the hook recorded the session: {out}");
            let polled = parse_poll(&std::fs::read_to_string(home.join("poll")).unwrap());
            assert_eq!(polled.len(), 1, "{shell}: {polled:?}");
            assert_eq!(polled[0].0, "tab-1");
            assert!(polled[0].1.alive, "{shell}: the recording shell was alive during the poll");
            assert_eq!(polled[0].1.state.session, "sess-1");
            assert_eq!(polled[0].1.state.cwd.as_deref(), Some("/srv/app"));
            // The shell has exited: the same record now reads as ended.
            let after = Command::new("/bin/sh").arg("-c").arg(poll_script()).env("HOME", &home).output().unwrap();
            let after = parse_poll(&String::from_utf8_lossy(&after.stdout));
            assert!(!after[0].1.alive, "{shell}: {after:?}");
            // Forgetting a closed tab removes its files and no one else's.
            std::fs::write(term.join("tab-2.history"), "x").unwrap();
            let forget = forget_script("tab-1").unwrap();
            Command::new("/bin/sh").arg("-c").arg(forget).env("HOME", &home).status().unwrap();
            assert!(!term.join("tab-1.history").exists() && !state.exists() && term.join("tab-2.history").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_home_still_gives_a_shell() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let ro = home.join("ro");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::create_dir(ro.join("probe")).is_ok() {
            return; // running as a user the permission does not bind
        }
        let out = run_tab(&ro, "/bin/sh", "tab-1", "echo still-a-shell\nexit\n");
        assert!(out.contains("still-a-shell"), "{out}");
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}
