//! Shell integration for Terminal tabs: each tab's own command history, its output kept for the
//! next start, and Claude Code session tracking, so a restored tab resumes exactly the
//! conversation that ran in it (`claude --resume <id>`).
//!
//! Per-tab files live under `<config>/terminals/<project>/` (never in the project: history and
//! output can hold secrets), named by the tab's stable key: `<key>.history`, `<key>.output`,
//! `<key>.claude.json`. The scripts below are written to `<config>/terminals/integration/`:
//!
//! - zsh: `ZDOTDIR` points at CodeCraft's startup files, which run the user's own (from their
//!   `ZDOTDIR` or `$HOME`) and then switch `HISTFILE` to the tab's file, after loading the
//!   user's history, so Up shows the tab's commands first and the rest after.
//! - bash: `bash --rcfile <bashrc> -i` does what a login shell does, then the same.
//! - fish: the tab's own `fish_history` session.
//! - A `claude` shim first on the tab's `PATH` starts the real `claude` with a `SessionStart`
//!   hook (`--settings`); the hook writes the session id and the claude process id to
//!   `$CODECRAFT_CLAUDE_STATE`. Claude Code tabs CodeCraft starts itself get the same hook.
//!
//! Sessions without a config directory of their own (tests, the CLI) get none of this unless
//! they set [`TerminalState::data_dir`](crate::terminal::TerminalState::data_dir) or call
//! [`Session::isolate_home`].

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::Session;

/// Saved output kept per tab (the end of the scrollback).
pub const MAX_OUTPUT_LINES: usize = 2000;
const MAX_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_STATE_BYTES: u64 = 64 * 1024;

/// The environment variables the scripts read.
pub const ENV_HISTFILE: &str = "CODECRAFT_HISTFILE";
pub const ENV_BIN: &str = "CODECRAFT_TERMINAL_BIN";
pub const ENV_CLAUDE_STATE: &str = "CODECRAFT_CLAUDE_STATE";
pub const ENV_CLAUDE_SETTINGS: &str = "CODECRAFT_CLAUDE_SETTINGS";
pub const ENV_CLAUDE_PID: &str = "CODECRAFT_CLAUDE_PID";
const ENV_USER_ZDOTDIR: &str = "CODECRAFT_USER_ZDOTDIR";
const ENV_ZDOTDIR_SET: &str = "CODECRAFT_ZDOTDIR_SET";

const HEADER: &str = "# CodeCraft terminal integration (generated: edits are overwritten).\n";

const ZSHENV: &str = r#"_codecraft_zdotdir=$ZDOTDIR
ZDOTDIR=${CODECRAFT_USER_ZDOTDIR:-$HOME}
[[ -r $ZDOTDIR/.zshenv ]] && source $ZDOTDIR/.zshenv
CODECRAFT_USER_ZDOTDIR=$ZDOTDIR
ZDOTDIR=$_codecraft_zdotdir
"#;

const ZPROFILE: &str = r#"ZDOTDIR=$CODECRAFT_USER_ZDOTDIR
[[ -r $ZDOTDIR/.zprofile ]] && source $ZDOTDIR/.zprofile
CODECRAFT_USER_ZDOTDIR=$ZDOTDIR
ZDOTDIR=$_codecraft_zdotdir
"#;

const ZSHRC: &str = r#"# macOS's /etc/zshrc put the history file under this directory; it belongs in the user's.
[[ $HISTFILE == $_codecraft_zdotdir/.zsh_history ]] && HISTFILE=$CODECRAFT_USER_ZDOTDIR/.zsh_history
ZDOTDIR=$CODECRAFT_USER_ZDOTDIR
[[ -r $ZDOTDIR/.zshrc ]] && source $ZDOTDIR/.zshrc
if [[ -n $CODECRAFT_HISTFILE ]]; then
  # The user's history now; zsh reads $HISTFILE (this tab's) after this file, on top of it.
  [[ -n $HISTFILE && -r $HISTFILE ]] && fc -R $HISTFILE
  HISTFILE=$CODECRAFT_HISTFILE
  (( SAVEHIST > 0 )) || SAVEHIST=10000
  (( HISTSIZE >= SAVEHIST )) || HISTSIZE=$SAVEHIST
  setopt inc_append_history
fi
[[ -n $CODECRAFT_TERMINAL_BIN ]] && path=($CODECRAFT_TERMINAL_BIN ${path:#$CODECRAFT_TERMINAL_BIN})
# Shells started from here (and .zlogin) use the user's own directory.
if [[ -n $CODECRAFT_ZDOTDIR_SET || $CODECRAFT_USER_ZDOTDIR != $HOME ]]; then
  export ZDOTDIR=$CODECRAFT_USER_ZDOTDIR
else
  unset ZDOTDIR
fi
unset _codecraft_zdotdir CODECRAFT_USER_ZDOTDIR CODECRAFT_ZDOTDIR_SET CODECRAFT_HISTFILE
"#;

const BASHRC: &str = r#"# Started as `bash --rcfile <this file> -i`: the startup files of a login shell, then this
# tab's own history on top of the user's.
[ -r /etc/profile ] && . /etc/profile
if [ -r ~/.bash_profile ]; then . ~/.bash_profile
elif [ -r ~/.bash_login ]; then . ~/.bash_login
elif [ -r ~/.profile ]; then . ~/.profile
fi
if [ -n "$CODECRAFT_HISTFILE" ]; then
  _codecraft_hist=${HISTFILE:-$HOME/.bash_history}
  [ -r "$_codecraft_hist" ] && history -r "$_codecraft_hist"
  HISTFILE=$CODECRAFT_HISTFILE
  unset _codecraft_hist
fi
[ -n "$CODECRAFT_TERMINAL_BIN" ] && PATH="$CODECRAFT_TERMINAL_BIN:$PATH"
unset CODECRAFT_HISTFILE
"#;

const CLAUDE_SHIM: &str = r#"# `claude` in a CodeCraft terminal tab: the real one, with a SessionStart hook that tells
# CodeCraft which session runs here, so the restored tab resumes it.
set -f
self_dir=$(cd "$(dirname "$0")" && pwd -P)
real=
IFS=:
for d in $PATH; do
  [ -n "$d" ] || continue
  [ "$(cd "$d" 2>/dev/null && pwd -P)" = "$self_dir" ] && continue
  if [ -x "$d/claude" ] && [ ! -d "$d/claude" ]; then real="$d/claude"; break; fi
done
unset IFS
set +f
if [ -z "$real" ]; then echo "claude: command not found" >&2; exit 127; fi
# Not for a claude started inside Claude Code, or a second one inside this one.
if [ -n "$CODECRAFT_CLAUDE_STATE" ] && [ -n "$CODECRAFT_CLAUDE_SETTINGS" ] && [ -z "$CODECRAFT_CLAUDE_PID" ] && [ -z "$CLAUDECODE" ]; then
  CODECRAFT_CLAUDE_PID=$$
  export CODECRAFT_CLAUDE_PID
  exec "$real" --settings "$CODECRAFT_CLAUDE_SETTINGS" "$@"
fi
exec "$real" "$@"
"#;

const CLAUDE_HOOK: &str = r#"# Claude Code's SessionStart hook in a CodeCraft terminal tab: records the session (and the
# claude process) running in the tab. Prints nothing: SessionStart output would reach the model.
[ -n "$CODECRAFT_CLAUDE_STATE" ] || exit 0
tmp="$CODECRAFT_CLAUDE_STATE.$$"
{ printf '{"pid":"%s","hook":' "${CODECRAFT_CLAUDE_PID:-}"; cat; printf '}\n'; } >"$tmp" 2>/dev/null &&
  mv -f "$tmp" "$CODECRAFT_CLAUDE_STATE" 2>/dev/null
exit 0
"#;

/// The root of the terminal files, if this session keeps any.
pub(crate) fn data_dir(s: &Session) -> Option<PathBuf> {
    if let Some(d) = &s.terminal.data_dir {
        return Some(d.clone());
    }
    // Tests and the CLI never write into the user's configuration.
    if !s.lsp_enabled {
        return None;
    }
    crate::session_store::config_dir().map(|d| d.join("terminals"))
}

/// The directory of the open project's per-tab files.
fn tab_dir(s: &Session) -> Option<PathBuf> {
    let root = data_dir(s)?;
    Some(match s.ws.project.as_ref() {
        Some(p) => root.join(crate::session_store::project_key(&p.root)),
        None => root.join("no-project"),
    })
}

/// A tab key or Claude session id from disk is used in file names and command lines: only
/// plain characters.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A new random key (a UUID).
pub(crate) fn new_key() -> String {
    codecraft_mcp::new_token().unwrap_or_else(|_| {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        format!("{:x}-{:x}", now.as_nanos(), std::process::id())
    })
}

pub(crate) fn history_file(s: &Session, key: &str) -> Option<PathBuf> {
    valid_id(key).then(|| tab_dir(s).map(|d| d.join(format!("{key}.history"))))?
}

pub(crate) fn output_file(s: &Session, key: &str) -> Option<PathBuf> {
    valid_id(key).then(|| tab_dir(s).map(|d| d.join(format!("{key}.output"))))?
}

pub(crate) fn claude_state_file(s: &Session, key: &str) -> Option<PathBuf> {
    valid_id(key).then(|| tab_dir(s).map(|d| d.join(format!("{key}.claude.json"))))?
}

/// The installed scripts.
#[derive(Clone, Debug)]
pub(crate) struct Integration {
    pub dir: PathBuf,
}

impl Integration {
    fn zdotdir(&self) -> PathBuf {
        self.dir.join("zsh")
    }
    fn bashrc(&self) -> PathBuf {
        self.dir.join("bashrc")
    }
    pub fn bin(&self) -> PathBuf {
        self.dir.join("bin")
    }
    fn hook(&self) -> PathBuf {
        self.dir.join("claude-session-hook.sh")
    }
    pub fn claude_settings(&self) -> PathBuf {
        self.dir.join("claude-settings.json")
    }
}

/// Write the scripts (once per session; unchanged files are left alone). `None` when this
/// session keeps no terminal files or they cannot be written.
pub(crate) fn install(s: &mut Session) -> Option<Integration> {
    let dir = data_dir(s)?.join("integration");
    let integration = Integration { dir };
    if s.terminal.installed.as_ref() == Some(&integration.dir) {
        return Some(integration);
    }
    let hook_command = format!("/bin/sh {}", codecraft_remote::sh_quote(&integration.hook().to_string_lossy()));
    let settings = serde_json::json!({
        "hooks": { "SessionStart": [{ "hooks": [{ "type": "command", "command": hook_command }] }] }
    });
    let settings = serde_json::to_string_pretty(&settings).unwrap_or_default();
    let files: [(PathBuf, String, bool); 7] = [
        (integration.zdotdir().join(".zshenv"), format!("{HEADER}{ZSHENV}"), false),
        (integration.zdotdir().join(".zprofile"), format!("{HEADER}{ZPROFILE}"), false),
        (integration.zdotdir().join(".zshrc"), format!("{HEADER}{ZSHRC}"), false),
        (integration.bashrc(), format!("{HEADER}{BASHRC}"), false),
        (integration.bin().join("claude"), format!("#!/bin/sh\n{HEADER}{CLAUDE_SHIM}"), true),
        (integration.hook(), format!("#!/bin/sh\n{HEADER}{CLAUDE_HOOK}"), true),
        (integration.claude_settings(), settings, false),
    ];
    for (path, text, executable) in files {
        if let Err(e) = write_if_changed(&path, &text, executable) {
            log::warn!("terminal integration: {e}");
            return None;
        }
    }
    s.terminal.installed = Some(integration.dir.clone());
    Some(integration)
}

fn write_if_changed(path: &Path, text: &str, executable: bool) -> Result<(), String> {
    if std::fs::read_to_string(path).is_ok_and(|old| old == text) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    codecraft_fs::write_text_file_atomic(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("cannot make {} executable: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellKind {
    Zsh,
    Bash,
    Fish,
    Other,
}

pub(crate) fn shell_kind(shell: &str) -> ShellKind {
    let name = Path::new(shell).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match name.trim_start_matches('-') {
        "zsh" => ShellKind::Zsh,
        "bash" => ShellKind::Bash,
        "fish" => ShellKind::Fish,
        _ => ShellKind::Other,
    }
}

/// How to start a shell tab with the integration: the program and arguments (`None`: the login
/// shell as usual) and the environment to add.
pub(crate) struct ShellLaunch {
    pub program: Option<(String, Vec<String>)>,
    pub env: Vec<(String, String)>,
}

pub(crate) fn shell_launch(s: &Session, integration: &Integration, shell: &str, key: &str) -> ShellLaunch {
    let mut env = common_env(s, integration, key);
    let history = history_file(s, key);
    if let Some(dir) = history.as_ref().and_then(|h| h.parent()) {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut program = None;
    match shell_kind(shell) {
        ShellKind::Zsh => {
            let user = std::env::var("ZDOTDIR").ok().filter(|d| !d.is_empty());
            if let Some(user) = &user {
                env.push((ENV_USER_ZDOTDIR.into(), user.clone()));
                env.push((ENV_ZDOTDIR_SET.into(), "1".into()));
            }
            env.push(("ZDOTDIR".into(), integration.zdotdir().to_string_lossy().into_owned()));
            if let Some(h) = &history {
                env.push((ENV_HISTFILE.into(), h.to_string_lossy().into_owned()));
            }
        }
        ShellKind::Bash => {
            if let Some(h) = &history {
                env.push((ENV_HISTFILE.into(), h.to_string_lossy().into_owned()));
            }
            let rc = integration.bashrc().to_string_lossy().into_owned();
            program = Some((shell.to_string(), vec!["--rcfile".into(), rc, "-i".into()]));
        }
        ShellKind::Fish => {
            let session: String = key.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
            let init = format!("set -g fish_history codecraft_{session}; set -gx PATH $CODECRAFT_TERMINAL_BIN $PATH");
            program = Some((shell.to_string(), vec!["-l".into(), "-i".into(), "-C".into(), init]));
        }
        ShellKind::Other => {}
    }
    ShellLaunch { program, env }
}

/// The environment every tab gets: where the `claude` shim is (the caller puts it first on
/// `PATH`) and where the Claude Code hook records the session.
pub(crate) fn common_env(s: &Session, integration: &Integration, key: &str) -> Vec<(String, String)> {
    let mut env = Vec::new();
    env.push((ENV_BIN.into(), integration.bin().to_string_lossy().into_owned()));
    env.push((ENV_CLAUDE_SETTINGS.into(), integration.claude_settings().to_string_lossy().into_owned()));
    if let Some(state) = claude_state_file(s, key) {
        env.push((ENV_CLAUDE_STATE.into(), state.to_string_lossy().into_owned()));
    }
    env
}

/// Remove the per-tab files of tabs other than `keys` (closed tabs, exited shells).
pub(crate) fn prune(s: &Session, keys: &[String]) {
    let Some(dir) = tab_dir(s) else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((key, rest)) = name.split_once('.') else { continue };
        let ours = matches!(rest, "history" | "output" | "claude.json") || rest.starts_with("claude.json.");
        if ours && valid_id(key) && !keys.iter().any(|k| k == key) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// What the Claude Code hook recorded for a tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeState {
    /// The claude process (`None`: the tab's own program, a Claude Code tab).
    pub pid: Option<u32>,
    pub session: String,
    pub transcript: Option<PathBuf>,
}

pub(crate) fn read_claude_state(path: &Path) -> Option<ClaudeState> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_STATE_BYTES {
        return None;
    }
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let session = v.pointer("/hook/session_id")?.as_str()?.to_string();
    if !valid_id(&session) {
        return None;
    }
    let pid = v.get("pid").and_then(Value::as_str).and_then(|p| p.parse::<u32>().ok()).filter(|p| *p > 0);
    let transcript =
        v.pointer("/hook/transcript_path").and_then(Value::as_str).filter(|p| p.starts_with('/')).map(PathBuf::from);
    Some(ClaudeState { pid, session, transcript })
}

/// The last [`MAX_OUTPUT_LINES`] lines of `text` (at most [`MAX_OUTPUT_BYTES`]), trailing blank
/// lines dropped.
pub(crate) fn output_tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let end = lines.iter().rposition(|l| !l.is_empty()).map_or(0, |i| i + 1);
    let lines = lines.get(..end).unwrap_or_default();
    let start = lines.len().saturating_sub(MAX_OUTPUT_LINES);
    let mut out = lines.get(start..).unwrap_or_default().join("\n");
    if out.len() > MAX_OUTPUT_BYTES {
        let mut cut = out.len() - MAX_OUTPUT_BYTES;
        while !out.is_char_boundary(cut) {
            cut += 1;
        }
        out = out.get(cut..).unwrap_or_default().to_string();
    }
    out
}

/// Saved output shown above a restored tab's new prompt (plain text, then a dim marker).
pub(crate) fn output_preface(saved: &str) -> Option<String> {
    let text = output_tail(saved);
    if text.is_empty() {
        return None;
    }
    // Only printable text and line breaks: the file came from disk.
    let clean: String = text.chars().filter(|c| *c == '\n' || !c.is_control()).collect();
    Some(format!("{}\r\n\x1b[2m── restored from the previous session ──\x1b[0m\r\n", clean.replace('\n', "\r\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_shells() {
        assert!(valid_id("2f6c1c9e-1d2b-4b8e-9a51-0c3d2e4f5a6b"));
        for bad in ["", "../x", "a b", "a/b", "x;rm", &"a".repeat(65)] {
            assert!(!valid_id(bad), "{bad}");
        }
        assert!(valid_id(&new_key()));
        assert_eq!(shell_kind("/bin/zsh"), ShellKind::Zsh);
        assert_eq!(shell_kind("-bash"), ShellKind::Bash);
        assert_eq!(shell_kind("/opt/homebrew/bin/fish"), ShellKind::Fish);
        assert_eq!(shell_kind("/bin/sh"), ShellKind::Other);
    }

    #[test]
    fn claude_state_is_read_defensively() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        std::fs::write(
            &p,
            r#"{"pid":"123","hook":{"session_id":"abc-1","transcript_path":"/t/abc-1.jsonl","source":"startup"}}"#,
        )
        .unwrap();
        assert_eq!(
            read_claude_state(&p),
            Some(ClaudeState { pid: Some(123), session: "abc-1".into(), transcript: Some("/t/abc-1.jsonl".into()) })
        );
        std::fs::write(&p, r#"{"pid":"","hook":{"session_id":"x"}}"#).unwrap();
        assert_eq!(read_claude_state(&p).unwrap().pid, None);
        for bad in [r#"{"pid":"1","hook":{"session_id":"$(rm -rf)"}}"#, r#"{"pid":"1","hook":"#, "", r#"{"hook":{}}"#] {
            std::fs::write(&p, bad).unwrap();
            assert_eq!(read_claude_state(&p), None, "{bad}");
        }
        assert_eq!(read_claude_state(&dir.path().join("missing")), None);
    }

    #[test]
    fn output_is_trimmed_and_cleaned() {
        assert_eq!(output_tail("a  \nb\n\n\n"), "a\nb");
        let many: String = (0..3000).map(|i| format!("{i}\n")).collect();
        let tail = output_tail(&many);
        assert!(tail.starts_with("1000\n") && tail.ends_with("2999"));
        let wide = "é".repeat(MAX_OUTPUT_BYTES);
        assert!(output_tail(&wide).len() <= MAX_OUTPUT_BYTES);
        let p = output_preface("ls\x1b[31m\x07\nfile\n").unwrap();
        assert!(p.starts_with("ls[31m\r\nfile\r\n"), "{p:?}");
        assert!(output_preface(" \n\n").is_none());
    }
}
