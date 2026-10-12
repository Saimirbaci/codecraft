//! Real PTY sessions: output → grid and colours, resize, alternate screen, exit status, and
//! process-tree termination (nothing may survive a closed terminal).
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use codecraft_term::{CellFlags, Color, Key, Mods, Palette, Screen, SelectionKind, TermConfig, TermEvent, Terminal};

fn spawn(program: &str, args: &[&str], cols: u16, rows: u16) -> Terminal {
    let cfg = TermConfig {
        program: Some(program.to_string()),
        args: args.iter().map(|a| (*a).to_string()).collect(),
        cols,
        rows,
        scrollback: 1000,
        ..TermConfig::default()
    };
    Terminal::spawn(cfg, Arc::new(Mutex::new(None)), Arc::new(RwLock::new(Palette::default()))).unwrap()
}

fn sh(script: &str) -> Terminal {
    spawn("/bin/sh", &["-c", script], 80, 24)
}

fn wait_screen(t: &Terminal, what: &str, mut done: impl FnMut(&Screen) -> bool) -> Screen {
    let start = Instant::now();
    loop {
        t.tick();
        let s = t.screen();
        if done(&s) {
            return s;
        }
        if start.elapsed() > Duration::from_secs(20) {
            panic!("timed out waiting for {what}; screen:\n{}", s.text());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_exit(t: &Terminal) -> codecraft_term::ExitInfo {
    let start = Instant::now();
    loop {
        for e in t.poll_events() {
            if let TermEvent::Exited(info) = e {
                return info;
            }
        }
        assert!(start.elapsed() < Duration::from_secs(20), "process did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn alive(pid: i32) -> bool {
    // `kill -0` semantics without unsafe code; zombies awaiting their parent count as dead.
    std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| {
            let stat = String::from_utf8_lossy(&o.stdout);
            let stat = stat.trim();
            !stat.is_empty() && !stat.starts_with('Z')
        })
        .unwrap_or(false)
}

fn wait_dead(pid: i32, what: &str) {
    let start = Instant::now();
    while alive(pid) {
        assert!(start.elapsed() < Duration::from_secs(5), "{what} ({pid}) survived the terminal");
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn pid_after(screen: &Screen, label: &str) -> i32 {
    let text = screen.text();
    let line = text.lines().find(|l| l.starts_with(label)).unwrap_or_else(|| panic!("no {label} in\n{text}"));
    line[label.len()..].trim().parse().unwrap()
}

#[test]
fn output_lands_in_the_grid_with_colours_and_attributes() {
    let t = sh(
        r"printf 'plain \033[31mred\033[0m \033[1;4mbold\033[0m \033[38;5;208mX\033[0m \033[38;2;1;2;3mT\033[48;2;9;8;7mB\033[0m \033[7mI\033[0m\n'; printf '\346\227\245\346\234\254\n'; sleep 5",
    );
    let s = wait_screen(&t, "output", |s| s.text().contains("日本"));
    assert_eq!(s.row_text(0), "plain red bold X TB I");
    let row = s.row(0);
    assert_eq!(row[0].fg, Color::Fg);
    assert_eq!(row[6].fg, Color::Indexed(1), "red");
    assert!(row[10].flags.has(CellFlags::BOLD) && row[10].flags.has(CellFlags::UNDERLINE));
    assert_eq!(row[15].fg, Color::Indexed(208), "256-colour");
    assert_eq!(row[17].fg, Color::Rgb(1, 2, 3), "truecolor fg");
    assert_eq!(row[18].bg, Color::Rgb(9, 8, 7), "truecolor bg");
    assert!(row[20].flags.has(CellFlags::INVERSE));
    // Wide characters take two cells.
    let wide = s.row(1);
    assert_eq!(wide[0].ch, '日');
    assert!(wide[0].flags.has(CellFlags::WIDE));
    assert!(wide[1].flags.has(CellFlags::SPACER));
    assert_eq!(wide[2].ch, '本');
    assert_eq!(s.row_text_with_cols(1).1, vec![0, 2]);
}

#[test]
fn environment_and_exit_status() {
    let cfg = TermConfig {
        program: Some("/bin/sh".into()),
        args: vec!["-c".into(), "echo \"term=$TERM prog=$TERM_PROGRAM color=$COLORTERM x=$X_TEST\"; exit 3".into()],
        env: vec![("X_TEST".into(), "42".into())],
        ..TermConfig::default()
    };
    let t = Terminal::spawn(cfg, Arc::new(Mutex::new(None)), Arc::new(RwLock::new(Palette::default()))).unwrap();
    let info = wait_exit(&t);
    assert_eq!(info.code, Some(3));
    assert!(t.exited());
    let s = wait_screen(&t, "env line", |s| s.text().contains("term="));
    assert_eq!(s.row_text(0), "term=xterm-256color prog=CodeCraft color=truecolor x=42");
}

#[test]
fn login_shell_in_the_requested_directory() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = TermConfig {
        shell: Some("/bin/sh".into()),
        cwd: Some(dir.path().to_path_buf()),
        env: vec![("PS1".into(), "$ ".into())],
        ..TermConfig::default()
    };
    let t = Terminal::spawn(cfg, Arc::new(Mutex::new(None)), Arc::new(RwLock::new(Palette::default()))).unwrap();
    t.send_text("echo \"arg0=$0\"; pwd -P\r");
    let canon = dir.path().canonicalize().unwrap();
    let s = wait_screen(&t, "pwd", |s| s.text().contains(&canon.display().to_string()));
    assert!(s.text().contains("arg0=-sh"), "a login shell has a dash in argv[0]:\n{}", s.text());
}

#[test]
fn resize_reaches_the_program() {
    let mut t = spawn("/bin/sh", &[], 80, 24);
    t.resize(100, 30, (8, 16));
    assert_eq!(t.size(), (100, 30));
    t.send_text("stty size\r");
    let s = wait_screen(&t, "stty size", |s| s.text().lines().any(|l| l.trim() == "30 100"));
    assert_eq!((s.cols, s.rows), (100, 30));
    // Absurd sizes are clamped, never a panic.
    t.resize(0, 0, (0, 0));
    assert_eq!(t.size(), (2, 1));
    t.resize(u16::MAX, u16::MAX, (u16::MAX, u16::MAX));
    assert_eq!(t.size(), (1000, 500));
}

#[test]
fn alternate_screen_enter_and_exit() {
    let t = sh(
        r"printf 'main screen\n'; sleep 0.5; printf '\033[?1049h\033[HALT SCREEN'; sleep 1; printf '\033[?1049l'; sleep 5",
    );
    let s = wait_screen(&t, "alt screen", |s| s.modes.alt_screen && s.text().contains("ALT SCREEN"));
    assert!(!s.text().contains("main screen"));
    let s = wait_screen(&t, "main screen again", |s| !s.modes.alt_screen && s.text().contains("main screen"));
    assert!(!s.text().contains("ALT SCREEN"));
}

#[test]
fn keys_selection_scrollback_and_clear() {
    let t = spawn("/bin/sh", &[], 40, 5);
    t.send_text("for i in 1 2 3 4 5 6 7 8 9 10; do echo line$i; done\r");
    wait_screen(&t, "lines", |s| s.text().contains("line10"));
    let s = t.screen();
    assert!(s.history > 0, "lines scrolled into the history");
    t.scroll(codecraft_term::Scroll::Top);
    let top = t.screen();
    assert_eq!(top.display_offset, top.history);
    assert!(t.text(true).contains("line1\nline2"), "{}", t.text(true));
    // Typing snaps back to the bottom.
    assert!(t.send_key(Key::Char('c'), Mods { ctrl: true, ..Mods::NONE }));
    wait_screen(&t, "scrolled to bottom", |s| s.display_offset == 0);
    // Select a word.
    let s = t.screen();
    let row = (0..s.rows).find(|r| s.row_text(*r) == "line10").unwrap();
    t.start_selection(SelectionKind::Word, row, 1, false);
    assert_eq!(t.selection_text().as_deref(), Some("line10"));
    assert!(t.screen().is_selected(row, 0));
    t.clear_selection();
    assert_eq!(t.selection_text(), None);
    t.select_all();
    assert!(t.selection_text().unwrap().contains("line1\n"));
    // Cmd+K: history gone, the cursor line stays.
    t.clear();
    let s = t.screen();
    assert_eq!(s.history, 0);
    assert!(!s.text().contains("line10"), "{}", s.text());
    assert_eq!(s.cursor.map(|c| c.row), Some(0));
}

#[test]
fn bracketed_paste_wraps_pasted_text() {
    // `cat -v` shows the bracket markers the terminal sends once the program enables 2004.
    let t = sh(r"printf '\033[?2004h'; stty -echo; cat -v");
    std::thread::sleep(Duration::from_millis(300));
    t.paste("a\nb\x1b[201~");
    // `cat` is line-buffered: a typed Enter flushes the last line.
    t.send_text("\r");
    let s = wait_screen(&t, "pasted", |s| s.text().contains("[201~^[[201~"));
    let text = s.text();
    assert!(text.contains("^[[200~a\nb[201~^[[201~"), "the inner end marker must lose its ESC:\n{text}");
}

fn tree_survivors(script: &str) {
    let mut t = sh(script);
    let s = wait_screen(&t, "pids", |s| s.text().contains("job="));
    let child = pid_after(&s, "child=");
    let job = pid_after(&s, "job=");
    let shell = t.pid().unwrap() as i32;
    assert!(alive(child) && alive(job) && alive(shell));
    let done = t.terminate();
    assert!(done.recv_timeout(Duration::from_secs(10)).is_err(), "terminate finishes by closing the channel");
    wait_dead(shell, "the shell");
    wait_dead(child, "the background child");
    wait_dead(job, "the job in its own process group");
}

#[test]
fn closing_kills_the_whole_process_tree() {
    // `set -m` gives the second job its own process group, like an interactive shell does.
    tree_survivors("sleep 1000 & echo \"child=$!\"; set -m; sleep 1001 & echo \"job=$!\"; wait");
}

#[test]
fn closing_kills_processes_that_ignore_sighup() {
    tree_survivors("trap '' HUP; sleep 1002 & echo \"child=$!\"; set -m; sleep 1003 & echo \"job=$!\"; wait");
}

#[test]
fn dropping_a_terminal_kills_it() {
    let t = sh("sleep 1004 & echo \"child=$!\"; wait");
    let s = wait_screen(&t, "pid", |s| s.text().contains("child="));
    let child = pid_after(&s, "child=");
    let shell = t.pid().unwrap() as i32;
    drop(t);
    wait_dead(shell, "the shell");
    wait_dead(child, "the child");
}

#[test]
fn a_missing_program_is_an_error() {
    let cfg = TermConfig { program: Some("/no/such/program".into()), ..TermConfig::default() };
    let err = Terminal::spawn(cfg, Arc::new(Mutex::new(None)), Arc::new(RwLock::new(Palette::default())));
    assert!(err.is_err());
    assert!(!Path::new("/no/such/program").exists());
}
