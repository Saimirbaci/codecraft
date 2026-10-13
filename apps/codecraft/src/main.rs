//! CodeCraft desktop app.
//!
//! `codecraft [--no-restore] [PATH]`: open PATH (a file or directory), else the last project(s)
//! with their editors. `--no-restore` starts without reopening any project or editor.

use std::ffi::OsString;
use std::path::PathBuf;

use codecraft_ui_egui::Launch;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    codecraft_ui_egui::launch(parse(std::env::args_os().skip(1)))
}

fn parse(args: impl Iterator<Item = OsString>) -> Launch {
    let mut launch = Launch::default();
    for arg in args {
        if arg == "--no-restore" {
            launch.no_restore = true;
        } else if arg.to_str().is_some_and(|a| a.starts_with("--")) {
            log::warn!("ignoring unknown option {}", arg.to_string_lossy());
        } else if launch.open.is_none() {
            launch.open = Some(PathBuf::from(arg));
        }
    }
    launch
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Launch {
        parse(a.iter().map(OsString::from))
    }

    #[test]
    fn parses_path_and_no_restore() {
        let l = args(&[]);
        assert!(l.open.is_none() && !l.no_restore);
        let l = args(&["--no-restore", "/p", "/q", "--bogus"]);
        assert!(l.no_restore);
        assert_eq!(l.open, Some(PathBuf::from("/p")));
        let l = args(&["/p"]);
        assert_eq!((l.open, l.no_restore), (Some(PathBuf::from("/p")), false));
    }
}
