//! The running processes a debugger can attach to (Run › Attach to Process…), from `ps`.

use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// Processes listed at most (a busy machine has thousands).
pub const MAX_PROCESSES: usize = 5000;

/// What kind of program a process runs, which picks the debug adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProcessKind {
    /// A native program (C, C++, Zig, …) under lldb-dap.
    #[default]
    Native,
    /// A Rust program (lldb-dap with Rust's formatters).
    Rust,
    /// A Python interpreter (debugpy).
    Python,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub user: String,
    /// The executable's file name.
    pub name: String,
    /// The full command line.
    pub command: String,
    pub kind: ProcessKind,
}

/// Guess the kind from a command line: Python interpreters, binaries under a Cargo `target/`
/// directory, else native.
pub fn guess_kind(command: &str) -> ProcessKind {
    let exe = command.split_whitespace().next().unwrap_or("");
    let name = Path::new(exe).file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if name.starts_with("python") || name == "pytest" {
        ProcessKind::Python
    } else if exe.contains("/target/debug/") || exe.contains("/target/release/") {
        ProcessKind::Rust
    } else {
        ProcessKind::Native
    }
}

/// Parse `ps axww -o pid=,user=,args=` output (one process per line: pid, user, command line).
pub fn parse_ps(output: &str) -> Vec<ProcessInfo> {
    let mut out = Vec::new();
    for line in output.lines() {
        let mut rest = line.trim_start();
        let pid_text: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let Ok(pid) = pid_text.parse::<u32>() else { continue };
        rest = rest.get(pid_text.len()..).unwrap_or("").trim_start();
        let user: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
        if user.is_empty() {
            continue;
        }
        let command = rest.get(user.len()..).unwrap_or("").trim().to_owned();
        if command.is_empty() {
            continue;
        }
        let exe = command.split_whitespace().next().unwrap_or("");
        let name = Path::new(exe).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| exe.into());
        let kind = guess_kind(&command);
        out.push(ProcessInfo { pid, user, name, command, kind });
        if out.len() >= MAX_PROCESSES {
            break;
        }
    }
    out
}

/// The processes of the machine, from `ps` (runs a process: call it off the UI thread).
pub fn list_processes() -> Result<Vec<ProcessInfo>, String> {
    let output = Command::new("ps")
        .args(["axww", "-o", "pid=,user=,args="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run ps: {e}"))?;
    if !output.status.success() {
        return Err(format!("ps failed ({})", output.status));
    }
    Ok(parse_ps(&String::from_utf8_lossy(&output.stdout)))
}

/// The current user's name (`$USER`, else `id -un`), to list their processes first.
pub fn current_user() -> Option<String> {
    std::env::var("USER").ok().filter(|u| !u.is_empty()).or_else(|| {
        let out = Command::new("id").arg("-un").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (!name.is_empty()).then_some(name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_output_is_parsed_leniently() {
        let text = "    1 root             /sbin/launchd\n\
                    \x20 812 alice   /Users/alice/proj/target/debug/app --port 80\n\
                    garbage line\n\
                    \x20 900 alice\n\
                    \x20 901 bob   /usr/bin/python3.12 -m http.server\n\
                    99999999999 x y\n\
                    \x20 902 carol  /Applications/My App.app/Contents/MacOS/My App\n";
        let ps = parse_ps(text);
        assert_eq!(ps.len(), 4, "{ps:?}");
        assert_eq!(ps[0].pid, 1);
        assert_eq!(ps[0].name, "launchd");
        assert_eq!(ps[1].user, "alice");
        assert_eq!(ps[1].command, "/Users/alice/proj/target/debug/app --port 80");
        assert_eq!(ps[1].kind, ProcessKind::Rust);
        assert_eq!(ps[2].kind, ProcessKind::Python);
        assert_eq!(ps[3].pid, 902);
        assert_eq!(ps[3].kind, ProcessKind::Native);
        assert!(parse_ps("").is_empty());
        assert!(parse_ps("é ü\n\u{0}").is_empty());
    }

    #[test]
    fn listing_includes_this_process() {
        if crate::discover::which("ps").is_none() {
            eprintln!("ps missing; skipping");
            return;
        }
        let me = std::process::id();
        let list = list_processes().unwrap();
        assert!(list.iter().any(|p| p.pid == me), "{} processes", list.len());
    }
}
