//! rustfmt over stdin/stdout.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

use crate::{CargoError, Result, locate};

pub(crate) fn rustfmt(source: &str, edition: &str, cwd: &Path) -> Result<String> {
    let program = locate::find_rustfmt();
    let edition = edition.trim();
    let mut args = Vec::new();
    if !edition.is_empty() {
        args.extend(["--edition", edition]);
    }
    args.extend(["--emit", "stdout"]);
    let mut command = Command::new(&program);
    command.args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if cwd.is_dir() {
        command.current_dir(cwd);
    }
    let mut child =
        command.spawn().map_err(|source| CargoError::Spawn { program: program.display().to_string(), source })?;

    // Write on another thread: rustfmt may fill its stdout pipe before it has read all of stdin.
    let writer = match child.stdin.take() {
        Some(mut stdin) => {
            let input = source.to_owned();
            match thread::Builder::new()
                .name("codecraft-rustfmt-stdin".into())
                .spawn(move || stdin.write_all(input.as_bytes()))
            {
                Ok(handle) => Some(handle),
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(e.into());
                }
            }
        }
        None => None,
    };
    let output = child.wait_with_output()?;
    let written = match writer.map(thread::JoinHandle::join) {
        None => Ok(()),
        Some(Ok(result)) => result,
        Some(Err(_)) => Err(std::io::Error::other("rustfmt stdin writer failed")),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stderr = if stderr.is_empty() { format!("exited with {}", output.status) } else { stderr };
        return Err(CargoError::Failed { command: format!("rustfmt {}", args.join(" ")), stderr });
    }
    // A successful exit without having read all the input would mean truncated output.
    written?;
    String::from_utf8(output.stdout).map_err(|e| CargoError::Parse(format!("rustfmt output is not UTF-8: {e}")))
}
