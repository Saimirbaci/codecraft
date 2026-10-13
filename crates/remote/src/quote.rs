//! POSIX shell quoting for commands that run on the remote host.
//!
//! `ssh host CMD` hands CMD to the remote user's login shell, so every word we put into it is
//! quoted here. Scripts are wrapped in `sh -c '…'` so they also work when the login shell is not
//! POSIX (fish, csh).

use crate::{RemoteError, RemoteResult};

/// Characters that never need quoting in a POSIX shell word.
fn is_plain(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c)
}

/// Quote one word for a POSIX shell: returned as is when every character is plain, otherwise
/// wrapped in single quotes (an embedded `'` becomes `'\''`). Empty strings become `''`.
pub fn sh_quote(arg: &str) -> String {
    if !arg.is_empty() && arg.chars().all(is_plain) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Quote a remote path, keeping a leading `~` or `~/` meaning the remote home directory.
pub fn quote_path(path: &str) -> String {
    if path == "~" {
        return "\"$HOME\"".into();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if rest.is_empty() {
            return "\"$HOME\"/".into();
        }
        return format!("\"$HOME\"/{}", sh_quote(rest));
    }
    sh_quote(path)
}

/// Words quoted and joined with spaces.
pub fn join(words: &[String]) -> String {
    words.iter().map(|w| sh_quote(w)).collect::<Vec<_>>().join(" ")
}

/// Whether `name` is a valid environment variable name.
pub fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// `sh -c '<script>'`: runs a POSIX script whatever the remote login shell is.
pub fn sh_c(script: &str) -> String {
    format!("sh -c {}", sh_quote(script))
}

/// The script that runs a program in a remote directory: `cd` there (failing loudly), pick up
/// `~/.cargo/env` when it exists (non-login shells do not read profiles), then `exec` the program
/// with the environment.
pub fn run_script(dir: &str, env: &[(String, String)], program: &str, args: &[String]) -> RemoteResult<String> {
    if program.is_empty() {
        return Err(RemoteError::Invalid("no program to run".into()));
    }
    let mut out = format!("cd {} || exit 1; ", quote_path(dir));
    out.push_str("if [ -f \"$HOME/.cargo/env\" ]; then . \"$HOME/.cargo/env\"; fi; exec ");
    if !env.is_empty() {
        out.push_str("env ");
        for (k, v) in env {
            if !is_env_name(k) {
                return Err(RemoteError::Invalid(format!("invalid environment variable name `{k}`")));
            }
            out.push_str(&sh_quote(&format!("{k}={v}")));
            out.push(' ');
        }
    }
    out.push_str(&sh_quote(program));
    for a in args {
        out.push(' ');
        out.push_str(&sh_quote(a));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_plain_and_special_words() {
        assert_eq!(sh_quote("abc/def-1.rs"), "abc/def-1.rs");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(sh_quote("a;b|c&d`e`"), "'a;b|c&d`e`'");
        assert_eq!(sh_quote("~/x"), "'~/x'");
        assert_eq!(sh_quote("naïve"), "'naïve'");
    }

    #[test]
    fn remote_paths_keep_the_home_directory() {
        assert_eq!(quote_path("~"), "\"$HOME\"");
        assert_eq!(quote_path("~/"), "\"$HOME\"/");
        assert_eq!(quote_path("~/my proj"), "\"$HOME\"/'my proj'");
        assert_eq!(quote_path("/srv/app"), "/srv/app");
        assert_eq!(quote_path("~other/x"), "'~other/x'");
    }

    #[test]
    fn run_script_quotes_every_word() {
        let env = vec![("RUST_LOG".to_string(), "debug x".to_string())];
        let s = run_script("~/proj dir", &env, "cargo", &["run".into(), "--".into(), "a'b".into()]).unwrap();
        assert_eq!(
            s,
            "cd \"$HOME\"/'proj dir' || exit 1; if [ -f \"$HOME/.cargo/env\" ]; then . \"$HOME/.cargo/env\"; fi; \
             exec env 'RUST_LOG=debug x' cargo run -- 'a'\\''b'"
        );
        assert!(run_script("/x", &[("A B".into(), "1".into())], "p", &[]).is_err());
        assert!(run_script("/x", &[], "", &[]).is_err());
        assert_eq!(sh_c("echo 'hi'"), "sh -c 'echo '\\''hi'\\'''");
    }

    #[test]
    fn env_names() {
        assert!(is_env_name("_A1"));
        assert!(!is_env_name("1A"));
        assert!(!is_env_name(""));
        assert!(!is_env_name("A-B"));
    }
}
